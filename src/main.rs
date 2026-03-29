use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    net::SocketAddr,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use axum::{
    extract::{Path as AxumPath, Request, State},
    http::{
        header::{AUTHORIZATION, RETRY_AFTER},
        HeaderValue, StatusCode,
    },
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Response, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::RwLock,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{info, warn};
use uuid::Uuid;

const APP_NAME: &str = "codex-openai-wrapper";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG")
                .unwrap_or_else(|_| "codex_openai_wrapper=debug,tower_http=info".to_string()),
        )
        .init();

    let config = AppConfig::from_env()?;
    config.ensure_directories().await?;
    let sessions = SessionStore::load(config.data_dir.join("sessions.json")).await?;
    let pricing = PricingConfig::load(&config).await?;

    let state = AppState {
        model_cache: ModelCache::new(config.model_cache_ttl_secs),
        pricing,
        rate_limiter: RateLimiter::new(config.rate_limit_requests, config.rate_limit_window_secs),
        config,
        sessions,
    };
    let auth_state = state.clone();
    let rate_state = state.clone();

    // The wrapper exposes OpenAI- and Anthropic-compatible HTTP routes, but every
    // request eventually funnels into a Codex CLI or app-server call.
    let api = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/v1/models", get(list_models))
        .route("/v1/auth/status", get(auth_status))
        .route("/v1/sessions", get(list_sessions))
        .route(
            "/v1/sessions/:session_id",
            get(get_session).delete(delete_session),
        )
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route_layer(middleware::from_fn_with_state(
            rate_state,
            enforce_rate_limit,
        ))
        .route_layer(middleware::from_fn_with_state(auth_state, require_api_key));

    let app = Router::new()
        .route("/", get(index))
        .merge(api)
        .with_state(state.clone())
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let addr: SocketAddr = state.config.bind_addr.parse()?;
    info!("Starting {APP_NAME} {APP_VERSION} on http://{addr}");
    info!("Codex home: {}", state.config.codex_home.display());
    info!("Codex cwd: {}", state.config.codex_cwd.display());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Clone)]
struct AppState {
    config: AppConfig,
    sessions: SessionStore,
    model_cache: ModelCache,
    rate_limiter: RateLimiter,
    pricing: Option<PricingConfig>,
}

#[derive(Clone, Debug)]
struct AppConfig {
    bind_addr: String,
    codex_path: String,
    codex_home: PathBuf,
    codex_cwd: PathBuf,
    data_dir: PathBuf,
    default_model: String,
    default_reasoning_effort: Option<String>,
    has_explicit_default_model: bool,
    model_cache_ttl_secs: u64,
    rate_limit_requests: u32,
    rate_limit_window_secs: u64,
    enable_responses_websockets: bool,
    enable_responses_websockets_v2: bool,
    wrapper_api_key: Option<String>,
    service_name: String,
    pricing_file: Option<PathBuf>,
}

impl AppConfig {
    fn from_env() -> AppResult<Self> {
        let cwd = env::current_dir().map_err(|err| {
            AppError::Internal(format!("failed to read current directory: {err}"))
        })?;

        let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8000);

        let codex_home = env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| cwd.join(".codex-home"));

        let data_dir = env::var("WRAPPER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| cwd.join(".wrapper-data"));

        let codex_cwd = env::var("CODEX_WRAPPER_CWD")
            .map(PathBuf::from)
            .unwrap_or_else(|_| cwd.clone());
        let configured_default_model = env::var("DEFAULT_MODEL").ok();
        let (default_model, default_reasoning_effort, has_explicit_default_model) =
            match configured_default_model.as_deref().map(str::trim) {
                Some(value) if !value.is_empty() => {
                    if let Some((model, effort)) = split_model_reasoning_alias(value) {
                        (model, Some(effort.to_string()), true)
                    } else {
                        (value.to_string(), None, true)
                    }
                }
                _ => ("gpt-5.4".to_string(), None, false),
            };

        // The wrapper uses a repo-local CODEX_HOME by default so Codex auth and
        // session state stay isolated from any global Codex install on the machine.
        Ok(Self {
            bind_addr: format!("{host}:{port}"),
            codex_path: env::var("CODEX_PATH").unwrap_or_else(|_| "codex".to_string()),
            codex_home,
            codex_cwd,
            data_dir,
            default_model,
            default_reasoning_effort,
            has_explicit_default_model,
            model_cache_ttl_secs: env::var("MODEL_CACHE_TTL_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30),
            rate_limit_requests: env::var("RATE_LIMIT_REQUESTS")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(120),
            rate_limit_window_secs: env::var("RATE_LIMIT_WINDOW_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(60),
            enable_responses_websockets: env_flag("CODEX_ENABLE_RESPONSES_WEBSOCKETS"),
            enable_responses_websockets_v2: env_flag("CODEX_ENABLE_RESPONSES_WEBSOCKETS_V2"),
            wrapper_api_key: env::var("WRAPPER_API_KEY").ok(),
            service_name: env::var("WRAPPER_SERVICE_NAME").unwrap_or_else(|_| APP_NAME.to_string()),
            pricing_file: env::var("WRAPPER_PRICING_FILE").ok().map(PathBuf::from),
        })
    }

    async fn ensure_directories(&self) -> AppResult<()> {
        fs::create_dir_all(&self.codex_home).await?;
        fs::create_dir_all(&self.data_dir).await?;
        fs::create_dir_all(self.data_dir.join("schemas")).await?;
        Ok(())
    }
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("rate limit exceeded")]
    RateLimited(u64),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Codex(String),
    #[error("{0}")]
    Internal(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

type AppResult<T> = Result<T, AppError>;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_type, code) = match &self {
            AppError::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "bad_request",
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "unauthorized",
            ),
            AppError::RateLimited(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "rate_limit_exceeded",
            ),
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, "invalid_request_error", "not_found"),
            AppError::Codex(message) => classify_codex_error(message),
            AppError::Internal(_) | AppError::Io(_) | AppError::Json(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal_error",
            ),
        };

        let mut response = (
            status,
            Json(json!({
                "error": {
                    "message": self.to_string(),
                    "type": error_type,
                    "code": code
                }
            })),
        )
            .into_response();

        if let AppError::RateLimited(retry_after_secs) = self {
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
        }

        response
    }
}

fn classify_codex_error(message: &str) -> (StatusCode, &'static str, &'static str) {
    let lower = message.to_ascii_lowercase();
    if lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("api key")
    {
        (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "codex_auth_error",
        )
    } else if lower.contains("context window") {
        (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "context_window_exceeded",
        )
    } else if lower.contains("rate limit") || lower.contains("usage limit") {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "usage_limit_exceeded",
        )
    } else if lower.contains("overloaded") || lower.contains("failed attempts") {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "provider_overloaded",
        )
    } else {
        (StatusCode::BAD_GATEWAY, "api_error", "codex_error")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionRecord {
    session_id: String,
    thread_id: String,
    model: String,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone)]
struct SessionStore {
    path: Arc<PathBuf>,
    inner: Arc<RwLock<HashMap<String, SessionRecord>>>,
}

impl SessionStore {
    async fn load(path: PathBuf) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // The wrapper persists only its external session_id -> Codex thread_id
        // mapping. Codex still owns the actual thread transcript/history.
        let sessions = if path.exists() {
            let raw = fs::read_to_string(&path)
                .await
                .unwrap_or_else(|_| "[]".to_string());
            let records: Vec<SessionRecord> = serde_json::from_str(&raw).unwrap_or_default();
            records
                .into_iter()
                .map(|record| (record.session_id.clone(), record))
                .collect()
        } else {
            HashMap::new()
        };

        Ok(Self {
            path: Arc::new(path),
            inner: Arc::new(RwLock::new(sessions)),
        })
    }

    async fn list(&self) -> Vec<SessionRecord> {
        let guard = self.inner.read().await;
        let mut records = guard.values().cloned().collect::<Vec<_>>();
        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        records
    }

    async fn get(&self, session_id: &str) -> Option<SessionRecord> {
        let guard = self.inner.read().await;
        guard.get(session_id).cloned()
    }

    async fn upsert(
        &self,
        session_id: String,
        thread_id: String,
        model: String,
    ) -> AppResult<SessionRecord> {
        let now = unix_timestamp();
        let mut guard = self.inner.write().await;

        let record = match guard.get(&session_id) {
            Some(existing) => SessionRecord {
                session_id: session_id.clone(),
                thread_id,
                model,
                created_at: existing.created_at,
                updated_at: now,
            },
            None => SessionRecord {
                session_id: session_id.clone(),
                thread_id,
                model,
                created_at: now,
                updated_at: now,
            },
        };

        guard.insert(session_id, record.clone());
        let snapshot = guard.values().cloned().collect::<Vec<_>>();
        drop(guard);

        self.persist(snapshot).await?;
        Ok(record)
    }

    async fn delete(&self, session_id: &str) -> AppResult<Option<SessionRecord>> {
        let mut guard = self.inner.write().await;
        let removed = guard.remove(session_id);
        let snapshot = guard.values().cloned().collect::<Vec<_>>();
        drop(guard);
        self.persist(snapshot).await?;
        Ok(removed)
    }

    async fn persist(&self, snapshot: Vec<SessionRecord>) -> AppResult<()> {
        let raw = serde_json::to_string_pretty(&snapshot)?;
        fs::write(self.path.as_ref(), raw).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct ModelCache {
    ttl_secs: u64,
    inner: Arc<RwLock<Option<CachedModels>>>,
}

#[derive(Clone)]
struct CachedModels {
    fetched_at: i64,
    models: Vec<CodexModel>,
}

impl ModelCache {
    fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            inner: Arc::new(RwLock::new(None)),
        }
    }

    async fn get_or_refresh(&self, config: &AppConfig) -> AppResult<Vec<CodexModel>> {
        let now = unix_timestamp();
        if let Some(cached) = self.inner.read().await.as_ref() {
            if now.saturating_sub(cached.fetched_at) < self.ttl_secs as i64 {
                return Ok(cached.models.clone());
            }
        }

        let models = fetch_models(config).await?;
        let snapshot = CachedModels {
            fetched_at: now,
            models: models.clone(),
        };
        *self.inner.write().await = Some(snapshot);
        Ok(models)
    }
}

#[derive(Clone)]
struct RateLimiter {
    max_requests: u32,
    window_secs: u64,
    inner: Arc<RwLock<HashMap<String, Vec<i64>>>>,
}

impl RateLimiter {
    fn new(max_requests: u32, window_secs: u64) -> Self {
        Self {
            max_requests,
            window_secs,
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn check(&self, key: &str) -> Result<(), u64> {
        if self.max_requests == 0 {
            return Ok(());
        }

        let now = unix_timestamp();
        let floor = now.saturating_sub(self.window_secs as i64);
        let mut guard = self.inner.write().await;
        let entries = guard.entry(key.to_string()).or_default();
        entries.retain(|timestamp| *timestamp >= floor);

        if entries.len() as u32 >= self.max_requests {
            let retry_after = entries
                .first()
                .map(|timestamp| ((*timestamp + self.window_secs as i64) - now).max(1) as u64)
                .unwrap_or(self.window_secs.max(1));
            return Err(retry_after);
        }

        entries.push(now);
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ModelPricing {
    input_per_million: f64,
    #[serde(default)]
    cached_input_per_million: Option<f64>,
    output_per_million: f64,
    #[serde(default)]
    reasoning_output_per_million: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PricingFile {
    Direct(HashMap<String, ModelPricing>),
    Wrapped {
        models: HashMap<String, ModelPricing>,
    },
}

#[derive(Clone)]
struct PricingConfig {
    source: Arc<String>,
    models: Arc<HashMap<String, ModelPricing>>,
}

impl PricingConfig {
    async fn load(config: &AppConfig) -> AppResult<Option<Self>> {
        let Some(path) = config.pricing_file.as_ref() else {
            return Ok(None);
        };

        let raw = fs::read_to_string(path).await?;
        let parsed: PricingFile = serde_json::from_str(&raw)?;
        let models = match parsed {
            PricingFile::Direct(models) => models,
            PricingFile::Wrapped { models } => models,
        };

        Ok(Some(Self {
            source: Arc::new(path.display().to_string()),
            models: Arc::new(models),
        }))
    }
}

#[derive(Debug, Deserialize, Clone)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    enable_tools: Option<bool>,
    #[serde(default)]
    enable_web_search: Option<bool>,
    #[serde(default)]
    working_directory: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    max_completion_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    stop: Option<StopSequences>,
    #[serde(default)]
    response_format: Option<ResponseFormatRequest>,
    #[serde(default)]
    tools: Option<Value>,
    #[serde(default)]
    tool_choice: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    include_wrapper_metadata: Option<bool>,
    #[serde(default)]
    stream_options: Option<Value>,
}

impl ChatCompletionRequest {
    fn enable_tools(&self) -> bool {
        self.enable_tools
            .unwrap_or(self.tools.is_some() || self.tool_choice.is_some())
    }

    fn enable_web_search(&self) -> bool {
        self.enable_web_search.unwrap_or(false)
    }

    fn effective_max_tokens(&self) -> Option<u32> {
        self.max_completion_tokens.or(self.max_tokens)
    }

    fn include_wrapper_metadata(&self) -> bool {
        self.include_wrapper_metadata.unwrap_or(false)
    }

    fn stop_sequences(&self) -> Vec<String> {
        match &self.stop {
            Some(StopSequences::Single(value)) => vec![value.clone()],
            Some(StopSequences::Multiple(values)) => values.clone(),
            None => Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum StopSequences {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseFormatRequest {
    Text,
    JsonObject,
    JsonSchema { json_schema: JsonSchemaConfig },
}

#[derive(Debug, Deserialize, Clone)]
struct JsonSchemaConfig {
    #[serde(default)]
    name: Option<String>,
    schema: Value,
    #[serde(default)]
    strict: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: MessageContent,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<MessagePart>),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
enum MessagePart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlPayload },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ImageUrlPayload {
    Raw(String),
    Object { url: String },
}

#[derive(Debug, Deserialize)]
struct AnthropicMessagesRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(default)]
    system: Option<AnthropicSystemPrompt>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    tools: Option<Value>,
    #[serde(default)]
    tool_choice: Option<Value>,
    #[serde(default)]
    thinking: Option<Value>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    enable_tools: Option<bool>,
    #[serde(default)]
    enable_web_search: Option<bool>,
    #[serde(default)]
    working_directory: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

impl AnthropicMessagesRequest {
    fn into_chat_completion(self) -> ChatCompletionRequest {
        let inferred_enable_tools = self.enable_tools.or_else(|| {
            if self.tools.is_some() || self.tool_choice.is_some() {
                Some(true)
            } else {
                None
            }
        });
        let inferred_reasoning_effort = self.reasoning_effort.clone().or_else(|| {
            if self.thinking.is_some() {
                Some("medium".to_string())
            } else {
                None
            }
        });
        let session_id = self.session_id.clone().or_else(|| {
            self.metadata
                .as_ref()
                .and_then(|value| value.get("session_id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        let mut messages = Vec::new();

        if let Some(system) = self.system.as_ref().map(render_anthropic_system_prompt) {
            if !system.trim().is_empty() {
                messages.push(ChatMessage {
                    role: "system".to_string(),
                    content: MessageContent::Text(system),
                });
            }
        }

        for message in self.messages {
            let rendered = render_anthropic_message_content(&message.content);
            messages.push(ChatMessage {
                role: message.role,
                content: MessageContent::Text(rendered),
            });
        }

        ChatCompletionRequest {
            model: self.model,
            messages,
            stream: self.stream,
            session_id,
            enable_tools: inferred_enable_tools,
            enable_web_search: self.enable_web_search,
            working_directory: self.working_directory,
            reasoning_effort: inferred_reasoning_effort,
            n: Some(1),
            max_tokens: self.max_tokens,
            max_completion_tokens: None,
            temperature: self.temperature,
            top_p: self.top_p,
            stop: self.stop_sequences.map(StopSequences::Multiple),
            response_format: None,
            tools: self.tools,
            tool_choice: self.tool_choice,
            metadata: self.metadata,
            user: None,
            include_wrapper_metadata: Some(true),
            stream_options: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicMessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Value,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Deserialize)]
struct AnthropicImageSource {
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default, rename = "type")]
    source_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AnthropicSystemPrompt {
    Text(String),
    Blocks(Vec<AnthropicSystemBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicSystemBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Serialize)]
struct ChatCompletionResponse {
    id: String,
    object: String,
    created: i64,
    model: String,
    choices: Vec<ChatChoice>,
    usage: UsageBlock,
    service_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wrapper: Option<WrapperMetadata>,
}

#[derive(Debug, Serialize)]
struct ChatChoice {
    index: u32,
    message: AssistantMessage,
    logprobs: Option<Value>,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct AssistantMessage {
    role: String,
    content: String,
    refusal: Option<Value>,
    annotations: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct UsageBlock {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    prompt_tokens_details: PromptTokensDetails,
    completion_tokens_details: CompletionTokensDetails,
}

#[derive(Debug, Serialize)]
struct PromptTokensDetails {
    cached_tokens: i64,
    audio_tokens: i64,
}

#[derive(Debug, Serialize)]
struct CompletionTokensDetails {
    reasoning_tokens: i64,
    audio_tokens: i64,
    accepted_prediction_tokens: i64,
    rejected_prediction_tokens: i64,
}

#[derive(Debug, Serialize, Clone)]
struct WrapperMetadata {
    thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    usage: WrapperUsageMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing_source: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct WrapperUsageMetadata {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct CodexModelListResult {
    data: Vec<CodexModel>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CodexModel {
    id: String,
    model: String,
    display_name: String,
    description: String,
    hidden: bool,
    is_default: bool,
    default_reasoning_effort: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexAccountResult {
    account: Option<CodexAccount>,
    requires_openai_auth: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
enum CodexAccount {
    #[serde(rename = "apiKey")]
    ApiKey,
    #[serde(rename = "chatgpt")]
    Chatgpt {
        email: String,
        #[serde(rename = "planType")]
        plan_type: String,
    },
}

#[derive(Debug)]
struct PromptBundle {
    prompt: String,
    developer_instructions: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct StreamingUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct JsonRpcMessage {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ExecTurnCompleted {
    usage: ExecUsage,
}

#[derive(Debug, Deserialize, Clone)]
struct ExecUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct ExecTurnFailed {
    error: ExecError,
}

#[derive(Debug, Deserialize)]
struct ExecError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ExecItemCompleted {
    item: ExecItem,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ExecItem {
    #[serde(rename = "agent_message")]
    AgentMessage {
        #[serde(rename = "id")]
        _id: String,
        text: String,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(rename = "id")]
        _id: String,
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug)]
struct ExecChatResult {
    thread_id: String,
    text: String,
    usage: ExecUsage,
}

struct AppServerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl AppServerProcess {
    async fn start(config: &AppConfig) -> AppResult<Self> {
        // Force websocket feature flags explicitly so wrapper behavior does not
        // drift with Codex defaults or user config in CODEX_HOME.
        let args = vec![
            "app-server".to_string(),
            "-c".to_string(),
            format!(
                "features.responses_websockets={}",
                config.enable_responses_websockets
            ),
            "-c".to_string(),
            format!(
                "features.responses_websockets_v2={}",
                config.enable_responses_websockets_v2
            ),
        ];
        let mut command = codex_command(config, &args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            AppError::Internal("failed to capture codex app-server stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AppError::Internal("failed to capture codex app-server stdout".to_string())
        })?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!("codex app-server stderr: {line}");
                }
            });
        }

        let mut process = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
        };
        process.initialize(config).await?;
        Ok(process)
    }

    async fn initialize(&mut self, config: &AppConfig) -> AppResult<()> {
        let init_result = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": config.service_name.clone(),
                        "title": "Codex OpenAI Wrapper",
                        "version": APP_VERSION
                    },
                    "capabilities": {
                        "experimentalApi": true
                    }
                }),
            )
            .await?;

        if init_result.get("userAgent").is_none() {
            return Err(AppError::Codex(
                "initialize returned an unexpected payload".to_string(),
            ));
        }

        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> AppResult<Value> {
        let request_id = self.next_id;
        self.next_id += 1;

        self.write_json(&json!({
            "method": method,
            "id": request_id,
            "params": params
        }))
        .await?;

        loop {
            let message = self.read_message().await?;
            if message.id.as_ref().and_then(Value::as_u64) == Some(request_id) {
                if let Some(error) = message.error {
                    let detail = error
                        .data
                        .map(|value| format!(" ({value})"))
                        .unwrap_or_default();
                    return Err(AppError::Codex(format!(
                        "{method} failed [{}]: {}{}",
                        error.code, error.message, detail
                    )));
                }

                return Ok(message.result.unwrap_or(Value::Null));
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> AppResult<()> {
        self.write_json(&json!({
            "method": method,
            "params": params
        }))
        .await
    }

    async fn read_message(&mut self) -> AppResult<JsonRpcMessage> {
        loop {
            match self.stdout.next_line().await? {
                Some(line) if !line.trim().is_empty() => return Ok(serde_json::from_str(&line)?),
                Some(_) => continue,
                None => {
                    return Err(AppError::Codex(
                        "codex app-server closed unexpectedly".to_string(),
                    ))
                }
            }
        }
    }

    async fn write_json(&mut self, value: &Value) -> AppResult<()> {
        let mut raw = serde_json::to_vec(value)?;
        raw.push(b'\n');
        self.stdin.write_all(&raw).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

async fn require_api_key(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(expected) = &state.config.wrapper_api_key {
        let bearer_token = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let header_token = request
            .headers()
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let token = bearer_token.or(header_token).unwrap_or_default();

        if token != expected {
            return AppError::Unauthorized.into_response();
        }
    }

    next.run(request).await
}

async fn enforce_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let key = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            request
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("anonymous")
        .to_string();

    if let Err(retry_after_secs) = state.rate_limiter.check(&key).await {
        return AppError::RateLimited(retry_after_secs).into_response();
    }

    next.run(request).await
}

async fn index(State(state): State<AppState>) -> Html<String> {
    let auth_hint = match &state.config.wrapper_api_key {
        Some(_) => "This server requires the configured wrapper API key in the Authorization header.",
        None => "This server accepts any Bearer token by default so OpenAI client libraries can connect without extra setup.",
    };

    let html = format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Codex OpenAI Wrapper</title>
    <style>
      :root {{
        --bg: #f4efe2;
        --panel: #fffaf0;
        --ink: #1f241f;
        --muted: #5a6258;
        --accent: #0c6c56;
        --border: #d8ceb7;
      }}
      * {{ box-sizing: border-box; }}
      body {{
        margin: 0;
        font-family: Georgia, "Iowan Old Style", "Palatino Linotype", serif;
        color: var(--ink);
        background:
          radial-gradient(circle at top left, rgba(12,108,86,0.14), transparent 28%),
          radial-gradient(circle at top right, rgba(170,102,67,0.14), transparent 24%),
          linear-gradient(180deg, #fbf6ea, #efe5d4);
      }}
      .wrap {{ max-width: 1080px; margin: 0 auto; padding: 32px 20px 60px; }}
      .hero, .card {{
        border: 1px solid var(--border);
        background: rgba(255,250,240,0.9);
        box-shadow: 0 12px 42px rgba(31,36,31,0.08);
      }}
      .hero {{ padding: 28px; display: grid; gap: 12px; }}
      .badge {{ display: inline-block; padding: 6px 10px; border: 1px solid var(--border); background: #fff; font-size: 0.82rem; text-transform: uppercase; }}
      h1 {{ margin: 0; font-size: clamp(2rem, 4vw, 4rem); line-height: 0.96; }}
      p {{ margin: 0; line-height: 1.5; color: var(--muted); }}
      .grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); gap: 18px; margin-top: 20px; }}
      .card {{ padding: 20px; }}
      .card h2 {{ margin: 0 0 10px; font-size: 1.15rem; }}
      .mono {{ font-family: "Cascadia Code", Consolas, monospace; white-space: pre-wrap; word-break: break-word; font-size: 0.92rem; }}
      .row {{ display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }}
      label {{ display: block; margin: 0 0 6px; font-weight: 600; }}
      input, textarea {{
        width: 100%;
        border: 1px solid var(--border);
        background: #fff;
        padding: 10px 12px;
        font: inherit;
      }}
      textarea {{ min-height: 140px; resize: vertical; }}
      .actions {{ display: flex; gap: 10px; flex-wrap: wrap; margin-top: 14px; }}
      button {{ border: 1px solid var(--ink); background: var(--ink); color: #fff; padding: 10px 14px; font: inherit; cursor: pointer; }}
      button.secondary {{ background: transparent; color: var(--ink); }}
      .output {{ margin-top: 12px; padding: 12px; border: 1px dashed var(--border); background: #fff; min-height: 110px; }}
      @media (max-width: 720px) {{ .row {{ grid-template-columns: 1fr; }} }}
    </style>
  </head>
  <body>
    <div class="wrap">
      <section class="hero">
        <span class="badge">Rust MVP • Codex-backed</span>
        <h1>Codex OpenAI Wrapper</h1>
        <p>An OpenAI-compatible HTTP facade for Codex. Existing OpenAI client libraries can point at this server instead of <code>api.openai.com</code>.</p>
        <p><strong>Auth:</strong> {auth_hint}</p>
      </section>
      <section class="grid">
        <div class="card"><h2>Service</h2><div class="mono" id="health">Loading...</div></div>
        <div class="card"><h2>Auth Status</h2><div class="mono" id="auth">Loading...</div></div>
        <div class="card"><h2>Models</h2><div class="mono" id="models">Loading...</div></div>
      </section>
      <section class="grid">
        <div class="card">
          <h2>Quick Start</h2>
          <div class="mono">from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8000/v1",
    api_key="anything-unless-wrapper-api-key-is-configured",
)

resp = client.chat.completions.create(
    model="{default_model}",
    messages=[{{"role": "user", "content": "Reply with hello"}}],
)</div>
        </div>
        <div class="card">
          <h2>API Explorer</h2>
          <label for="apiKey">Authorization Bearer Token</label>
          <input id="apiKey" placeholder="Only needed if WRAPPER_API_KEY is configured" />
          <div class="row" style="margin-top: 12px;">
            <div><label for="model">Model</label><input id="model" value="{default_model}" /></div>
            <div><label for="sessionId">Session ID</label><input id="sessionId" placeholder="optional-session-id" /></div>
          </div>
          <label for="prompt" style="margin-top: 12px;">Prompt</label>
          <textarea id="prompt">Explain what this wrapper does in one paragraph.</textarea>
          <div class="actions">
            <button id="run">Send Request</button>
            <button class="secondary" id="stream">Stream Request</button>
          </div>
          <div class="output mono" id="result"></div>
        </div>
      </section>
    </div>
    <script>
      const byId = (id) => document.getElementById(id);
      async function fetchJson(path) {{
        const token = byId("apiKey").value.trim();
        const headers = token ? {{ Authorization: `Bearer ${{token}}` }} : {{}};
        const resp = await fetch(path, {{ headers }});
        return await resp.json();
      }}
      async function loadPanels() {{
        byId("health").textContent = JSON.stringify(await fetchJson("/health"), null, 2);
        byId("auth").textContent = JSON.stringify(await fetchJson("/v1/auth/status"), null, 2);
        byId("models").textContent = JSON.stringify(await fetchJson("/v1/models"), null, 2);
      }}
      async function runChat(stream) {{
        const token = byId("apiKey").value.trim();
        const headers = {{ "Content-Type": "application/json" }};
        if (token) headers["Authorization"] = `Bearer ${{token}}`;
        const body = {{
          model: byId("model").value.trim(),
          messages: [{{ role: "user", content: byId("prompt").value }}],
          stream,
          session_id: byId("sessionId").value.trim() || null
        }};
        const result = byId("result");
        result.textContent = stream ? "" : "Sending...";
        const resp = await fetch("/v1/chat/completions", {{ method: "POST", headers, body: JSON.stringify(body) }});
        if (!stream) {{
          result.textContent = JSON.stringify(await resp.json(), null, 2);
          return;
        }}
        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        while (true) {{
          const {{ value, done }} = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, {{ stream: true }});
          let boundary = buffer.indexOf("\\n\\n");
          while (boundary !== -1) {{
            const chunk = buffer.slice(0, boundary);
            buffer = buffer.slice(boundary + 2);
            boundary = buffer.indexOf("\\n\\n");
            if (!chunk.startsWith("data: ")) continue;
            const payload = chunk.slice(6).trim();
            if (payload === "[DONE]") continue;
            const json = JSON.parse(payload);
            const delta = json.choices?.[0]?.delta?.content;
            if (delta) result.textContent += delta;
          }}
        }}
      }}
      byId("run").addEventListener("click", () => runChat(false));
      byId("stream").addEventListener("click", () => runChat(true));
      loadPanels().catch((error) => {{ byId("health").textContent = String(error); }});
    </script>
  </body>
</html>"#,
        auth_hint = auth_hint,
        default_model = state.config.default_model,
    );

    Html(html)
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": APP_NAME,
        "version": APP_VERSION,
        "codex_path": state.config.codex_path,
        "codex_home": state.config.codex_home,
        "codex_cwd": state.config.codex_cwd,
        "rate_limit": {
            "max_requests": state.config.rate_limit_requests,
            "window_secs": state.config.rate_limit_window_secs
        },
        "pricing_enabled": state.pricing.is_some(),
    }))
}

async fn version() -> Json<Value> {
    Json(json!({
        "name": APP_NAME,
        "version": APP_VERSION
    }))
}

async fn list_models(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let models = fetch_cached_models(&state).await?;
    let data = models
        .into_iter()
        .filter(|model| !model.hidden)
        .map(|model| {
            json!({
                "id": model.model,
                "object": "model",
                "created": 0,
                "owned_by": "openai",
                "provider": "codex",
                "codex_id": model.id,
                "display_name": model.display_name,
                "description": model.description,
                "default_reasoning_effort": model.default_reasoning_effort,
                "is_default": model.is_default,
                "supports_streaming": true,
                "supports_response_format": true
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(json!({
        "object": "list",
        "data": data
    })))
}

async fn auth_status(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let account = fetch_account(&state.config).await?;
    Ok(Json(json!({
        "authenticated": account.account.is_some(),
        "requires_openai_auth": account.requires_openai_auth,
        "account": account.account
    })))
}

fn request_messages_for_log(request: &ChatCompletionRequest) -> Vec<Value> {
    request
        .messages
        .iter()
        .map(|message| {
            json!({
                "role": message.role,
                "content": normalize_message_text(message)
            })
        })
        .collect()
}

fn model_log_value(model: Option<&CodexModel>) -> Value {
    match model {
        Some(model) => json!({
            "id": model.model,
            "codex_id": model.id,
            "display_name": model.display_name,
            "description": model.description,
            "provider": "codex",
            "owned_by": "openai",
            "is_default": model.is_default,
            "default_reasoning_effort": model.default_reasoning_effort,
            "supports_streaming": true,
            "supports_response_format": true,
            "hidden": model.hidden
        }),
        None => Value::Null,
    }
}

async fn lookup_model_for_log(state: &AppState, resolved_model: &str) -> Option<CodexModel> {
    fetch_cached_models(state)
        .await
        .ok()?
        .into_iter()
        .find(|model| model.model.eq_ignore_ascii_case(resolved_model))
}

fn response_format_for_log(request: &ChatCompletionRequest) -> &'static str {
    match request.response_format.as_ref() {
        Some(ResponseFormatRequest::Text) => "text",
        Some(ResponseFormatRequest::JsonObject) => "json_object",
        Some(ResponseFormatRequest::JsonSchema { .. }) => "json_schema",
        None => "default",
    }
}

fn log_request_summary(
    api: &str,
    requested_model: &str,
    requested_reasoning_effort: Option<&str>,
    request: &ChatCompletionRequest,
    prompt_bundle: &PromptBundle,
    resumed_session: bool,
    model: Option<&CodexModel>,
) {
    let payload = json!({
        "event": "wrapper_request",
        "api": api,
        "stream": request.stream,
        "session_id": request.session_id,
        "resumed_session": resumed_session,
        "requested_model": requested_model,
        "resolved_model": request.model,
        "requested_reasoning_effort": requested_reasoning_effort,
        "resolved_reasoning_effort": request.reasoning_effort,
        "enable_tools": request.enable_tools(),
        "enable_web_search": request.enable_web_search(),
        "working_directory": request.working_directory,
        "response_format": response_format_for_log(request),
        "messages": request_messages_for_log(request),
        "model_details": model_log_value(model),
        "codex_prompt": prompt_bundle.prompt,
        "codex_developer_instructions": prompt_bundle.developer_instructions
    });
    info!("{payload}");
}

fn log_response_summary(
    api: &str,
    request: &ChatCompletionRequest,
    model: Option<&CodexModel>,
    stream: bool,
    thread_id: &str,
    assistant_text: &str,
    wrapper: Option<&WrapperMetadata>,
) {
    let payload = json!({
        "event": "wrapper_response",
        "api": api,
        "stream": stream,
        "session_id": request.session_id,
        "thread_id": thread_id,
        "resolved_model": request.model,
        "resolved_reasoning_effort": request.reasoning_effort,
        "assistant_text": assistant_text,
        "model_details": model_log_value(model),
        "wrapper": wrapper
    });
    info!("{payload}");
}

fn log_response_error_summary(
    api: &str,
    request: &ChatCompletionRequest,
    model: Option<&CodexModel>,
    stream: bool,
    thread_id: Option<&str>,
    error_text: &str,
    partial_assistant_text: &str,
) {
    let payload = json!({
        "event": "wrapper_response_error",
        "api": api,
        "stream": stream,
        "session_id": request.session_id,
        "thread_id": thread_id,
        "resolved_model": request.model,
        "resolved_reasoning_effort": request.reasoning_effort,
        "error": error_text,
        "partial_assistant_text": partial_assistant_text,
        "model_details": model_log_value(model)
    });
    info!("{payload}");
}

async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let sessions = state.sessions.list().await;
    Json(json!({ "object": "list", "data": sessions }))
}

async fn get_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let session = state
        .sessions
        .get(&session_id)
        .await
        .ok_or_else(|| AppError::NotFound(format!("unknown session `{session_id}`")))?;

    Ok(Json(json!(session)))
}

async fn delete_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let removed = state
        .sessions
        .delete(&session_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("unknown session `{session_id}`")))?;

    Ok(Json(json!({
        "deleted": true,
        "session": removed,
        "note": "This removes the wrapper mapping. It does not delete Codex's persisted thread rollout."
    })))
}

fn validate_chat_request_shape(request: &ChatCompletionRequest) -> AppResult<()> {
    if request.messages.is_empty() {
        return Err(AppError::BadRequest(
            "`messages` must not be empty".to_string(),
        ));
    }

    if let Some(n) = request.n {
        if n != 1 {
            return Err(AppError::BadRequest(
                "This wrapper currently supports only `n = 1`.".to_string(),
            ));
        }
    }

    Ok(())
}

fn canonical_reasoning_effort(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        _ => None,
    }
}

fn split_model_reasoning_alias(requested: &str) -> Option<(String, &'static str)> {
    let trimmed = requested.trim();
    let (base, suffix) = trimmed.rsplit_once('-')?;
    let effort = canonical_reasoning_effort(suffix)?;
    let base = base.trim();
    if base.is_empty() {
        return None;
    }
    Some((base.to_string(), effort))
}

fn normalize_model_and_reasoning(request: &mut ChatCompletionRequest) -> AppResult<()> {
    // OpenAI-compatible clients sometimes only expose a `model` selector. Accept
    // aliases like `gpt-5.4-xhigh` and translate them into the canonical model id
    // plus an explicit reasoning effort before model validation runs.
    request.model = request.model.trim().to_string();
    request.reasoning_effort = request
        .reasoning_effort
        .take()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(explicit) = request.reasoning_effort.as_deref() {
        if let Some(canonical) = canonical_reasoning_effort(explicit) {
            request.reasoning_effort = Some(canonical.to_string());
        }
    }

    if let Some((base_model, alias_effort)) = split_model_reasoning_alias(&request.model) {
        if let Some(explicit_effort) = request.reasoning_effort.as_deref() {
            if !explicit_effort.eq_ignore_ascii_case(alias_effort) {
                return Err(AppError::BadRequest(format!(
                    "Model alias `{}` implies reasoning_effort `{alias_effort}`, which conflicts with explicit reasoning_effort `{explicit_effort}`.",
                    request.model
                )));
            }
        }

        request.model = base_model;
        request.reasoning_effort = Some(alias_effort.to_string());
    }

    Ok(())
}

fn apply_configured_default_model(config: &AppConfig, request: &mut ChatCompletionRequest) {
    let requested_model = request.model.trim();
    if !config.has_explicit_default_model
        || (!requested_model.is_empty() && !requested_model.eq_ignore_ascii_case("default"))
    {
        return;
    }

    request.model = config.default_model.clone();
    if request.reasoning_effort.is_none() {
        request.reasoning_effort = config.default_reasoning_effort.clone();
    }
}

async fn anthropic_messages(
    State(state): State<AppState>,
    Json(request): Json<AnthropicMessagesRequest>,
) -> AppResult<Response> {
    // Anthropic requests are normalized into the internal chat-completions shape
    // so both HTTP APIs share the same Codex translation layer.
    let mut request = request.into_chat_completion();
    let requested_model = request.model.clone();
    let requested_reasoning_effort = request.reasoning_effort.clone();
    normalize_model_and_reasoning(&mut request)?;
    apply_configured_default_model(&state.config, &mut request);
    validate_chat_request_shape(&request)?;
    request.model = resolve_model(&state, &request.model).await?;

    let existing_session = match &request.session_id {
        Some(session_id) => state.sessions.get(session_id).await,
        None => None,
    };
    let prompt_bundle = build_prompt_bundle(&request, existing_session.is_some())?;
    let model_details = lookup_model_for_log(&state, &request.model).await;
    log_request_summary(
        "anthropic_messages",
        &requested_model,
        requested_reasoning_effort.as_deref(),
        &request,
        &prompt_bundle,
        existing_session.is_some(),
        model_details.as_ref(),
    );

    if request.stream {
        let streaming = start_streaming_session(
            &state.config,
            &request,
            existing_session.as_ref(),
            &prompt_bundle,
        )
        .await?;

        if let Some(session_id) = request.session_id.clone() {
            let _ = state
                .sessions
                .upsert(
                    session_id,
                    streaming.thread_id.clone(),
                    request.model.clone(),
                )
                .await?;
        }

        let message_id = format!("msg_{}", Uuid::new_v4().simple());
        let model = request.model.clone();
        let session_id = request.session_id.clone();
        let sessions = state.sessions.clone();
        let thread_id = streaming.thread_id.clone();
        let stream_state = state.clone();
        let stream_request = request.clone();
        let stream_model_details = model_details.clone();

        let event_stream = stream! {
            let mut process = streaming.process;
            let mut sent_any_text = false;
            let mut usage = StreamingUsage::default();
            let mut response_text = String::new();

            let message_start = json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0
                    }
                }
            });
            yield Ok::<Event, Infallible>(
                Event::default()
                    .event("message_start")
                    .data(message_start.to_string()),
            );

            let content_start = json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            });
            yield Ok::<Event, Infallible>(
                Event::default()
                    .event("content_block_start")
                    .data(content_start.to_string()),
            );

            loop {
                let message = match process.read_message().await {
                    Ok(message) => message,
                    Err(error) => {
                        warn!("stream read failed: {error}");
                        break;
                    }
                };

                match message.method.as_deref() {
                    Some("item/agentMessage/delta") => {
                        if let Some(delta) = message
                            .params
                            .as_ref()
                            .and_then(|value| value.get("delta"))
                            .and_then(Value::as_str)
                        {
                            sent_any_text = true;
                            response_text.push_str(delta);
                            let event = json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {
                                    "type": "text_delta",
                                    "text": delta
                                }
                            });
                            yield Ok::<Event, Infallible>(
                                Event::default()
                                    .event("content_block_delta")
                                    .data(event.to_string()),
                            );
                        }
                    }
                    Some("item/completed") => {
                        let is_agent_message = message
                            .params
                            .as_ref()
                            .and_then(|value| value.pointer("/item/type"))
                            .and_then(Value::as_str)
                            == Some("agentMessage");

                        if is_agent_message && !sent_any_text {
                            if let Some(text) = message
                                .params
                                .as_ref()
                                .and_then(|value| value.pointer("/item/text"))
                                .and_then(Value::as_str)
                            {
                                sent_any_text = true;
                                response_text.push_str(text);
                                let event = json!({
                                    "type": "content_block_delta",
                                    "index": 0,
                                    "delta": {
                                        "type": "text_delta",
                                        "text": text
                                    }
                                });
                                yield Ok::<Event, Infallible>(
                                    Event::default()
                                        .event("content_block_delta")
                                        .data(event.to_string()),
                                );
                            }
                        }
                    }
                    Some("thread/tokenUsage/updated") => {
                        if let Some(params) = message.params.as_ref() {
                            update_streaming_usage(&mut usage, params);
                        }
                    }
                    Some("turn/completed") => {
                        if let Some(session_id) = session_id.clone() {
                            let _ = sessions.upsert(session_id, thread_id.clone(), model.clone()).await;
                        }

                        let wrapper = if stream_request.include_wrapper_metadata() {
                            build_wrapper_metadata(
                                &stream_state,
                                &stream_request,
                                thread_id.clone(),
                                wrapper_usage_from_stream(&usage),
                            )
                        } else {
                            None
                        };
                        log_response_summary(
                            "anthropic_messages",
                            &stream_request,
                            stream_model_details.as_ref(),
                            true,
                            &thread_id,
                            &response_text,
                            wrapper.as_ref(),
                        );

                        let content_stop = json!({
                            "type": "content_block_stop",
                            "index": 0
                        });
                        yield Ok::<Event, Infallible>(
                            Event::default()
                                .event("content_block_stop")
                                .data(content_stop.to_string()),
                        );

                        let message_delta = json!({
                            "type": "message_delta",
                            "delta": {
                                "stop_reason": "end_turn",
                                "stop_sequence": Value::Null
                            },
                            "usage": {
                                "output_tokens": usage.output_tokens
                            },
                            "wrapper": wrapper
                        });
                        yield Ok::<Event, Infallible>(
                            Event::default()
                                .event("message_delta")
                                .data(message_delta.to_string()),
                        );

                        let message_stop = json!({ "type": "message_stop" });
                        yield Ok::<Event, Infallible>(
                            Event::default()
                                .event("message_stop")
                                .data(message_stop.to_string()),
                        );
                        break;
                    }
                    Some("error") => {
                        let will_retry = message
                            .params
                            .as_ref()
                            .and_then(|value| value.get("willRetry"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);

                        if will_retry {
                            continue;
                        }

                        let error_text = message
                            .params
                            .as_ref()
                            .and_then(|value| value.pointer("/error/message"))
                            .and_then(Value::as_str)
                            .unwrap_or("Codex reported an error.");
                        log_response_error_summary(
                            "anthropic_messages",
                            &stream_request,
                            stream_model_details.as_ref(),
                            true,
                            Some(&thread_id),
                            error_text,
                            &response_text,
                        );
                        let error_event = json!({
                            "type": "error",
                            "error": {
                                "type": "api_error",
                                "message": error_text
                            }
                        });
                        yield Ok::<Event, Infallible>(
                            Event::default()
                                .event("error")
                                .data(error_event.to_string()),
                        );
                        break;
                    }
                    _ => {}
                }
            }

            process.shutdown().await;
        };

        let sse = Sse::new(event_stream)
            .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)));
        return Ok(sse.into_response());
    }

    let exec_result = run_exec_chat(
        &state.config,
        &request,
        existing_session.as_ref(),
        &prompt_bundle,
    )
    .await?;

    if let Some(session_id) = request.session_id.clone() {
        let _ = state
            .sessions
            .upsert(
                session_id,
                exec_result.thread_id.clone(),
                request.model.clone(),
            )
            .await?;
    }

    let wrapper = build_wrapper_metadata(
        &state,
        &request,
        exec_result.thread_id.clone(),
        wrapper_usage_from_exec(&exec_result.usage),
    );
    log_response_summary(
        "anthropic_messages",
        &request,
        model_details.as_ref(),
        false,
        &exec_result.thread_id,
        &exec_result.text,
        wrapper.as_ref(),
    );
    let response = json!({
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": request.model,
        "content": [{
            "type": "text",
            "text": exec_result.text
        }],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": exec_result.usage.input_tokens,
            "output_tokens": exec_result.usage.output_tokens
        },
        "wrapper": wrapper
    });

    Ok(Json(response).into_response())
}

async fn chat_completions(
    State(state): State<AppState>,
    Json(request): Json<ChatCompletionRequest>,
) -> AppResult<Response> {
    let mut request = request;
    let requested_model = request.model.clone();
    let requested_reasoning_effort = request.reasoning_effort.clone();
    normalize_model_and_reasoning(&mut request)?;
    apply_configured_default_model(&state.config, &mut request);
    validate_chat_request_shape(&request)?;
    request.model = resolve_model(&state, &request.model).await?;

    let existing_session = match &request.session_id {
        Some(session_id) => state.sessions.get(session_id).await,
        None => None,
    };
    let prompt_bundle = build_prompt_bundle(&request, existing_session.is_some())?;
    let model_details = lookup_model_for_log(&state, &request.model).await;
    log_request_summary(
        "chat_completions",
        &requested_model,
        requested_reasoning_effort.as_deref(),
        &request,
        &prompt_bundle,
        existing_session.is_some(),
        model_details.as_ref(),
    );

    if request.stream {
        let streaming = start_streaming_session(
            &state.config,
            &request,
            existing_session.as_ref(),
            &prompt_bundle,
        )
        .await?;

        if let Some(session_id) = request.session_id.clone() {
            let _ = state
                .sessions
                .upsert(
                    session_id,
                    streaming.thread_id.clone(),
                    request.model.clone(),
                )
                .await?;
        }

        let chat_id = format!("chatcmpl-{}", Uuid::new_v4());
        let created = unix_timestamp();
        let model = request.model.clone();
        let session_id = request.session_id.clone();
        let sessions = state.sessions.clone();
        let thread_id = streaming.thread_id.clone();
        let include_usage = request
            .stream_options
            .as_ref()
            .and_then(|value| value.get("include_usage"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let stream_state = state.clone();
        let stream_request = request.clone();
        let stream_model_details = model_details.clone();

        let event_stream = stream! {
            let mut process = streaming.process;
            let mut sent_any_text = false;
            let mut usage = StreamingUsage::default();
            let mut response_text = String::new();

            let initial = json!({
                "id": chat_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "role": "assistant", "content": "" },
                    "logprobs": Value::Null,
                    "finish_reason": Value::Null
                }]
            });
            yield Ok::<Event, Infallible>(Event::default().data(initial.to_string()));

            loop {
                let message = match process.read_message().await {
                    Ok(message) => message,
                    Err(error) => {
                        warn!("stream read failed: {error}");
                        break;
                    }
                };

                match message.method.as_deref() {
                    Some("item/agentMessage/delta") => {
                        if let Some(delta) = message
                            .params
                            .as_ref()
                            .and_then(|value| value.get("delta"))
                            .and_then(Value::as_str)
                        {
                            sent_any_text = true;
                            response_text.push_str(delta);
                            let chunk = json!({
                                "id": chat_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": { "content": delta },
                                    "logprobs": Value::Null,
                                    "finish_reason": Value::Null
                                }]
                            });
                            yield Ok::<Event, Infallible>(Event::default().data(chunk.to_string()));
                        }
                    }
                    Some("thread/tokenUsage/updated") => {
                        if let Some(params) = message.params.as_ref() {
                            update_streaming_usage(&mut usage, params);
                        }
                    }
                    Some("item/completed") => {
                        let is_agent_message = message
                            .params
                            .as_ref()
                            .and_then(|value| value.pointer("/item/type"))
                            .and_then(Value::as_str)
                            == Some("agentMessage");

                        if is_agent_message && !sent_any_text {
                            if let Some(text) = message
                                .params
                                .as_ref()
                                .and_then(|value| value.pointer("/item/text"))
                                .and_then(Value::as_str)
                            {
                                sent_any_text = true;
                                response_text.push_str(text);
                                let chunk = json!({
                                    "id": chat_id,
                                    "object": "chat.completion.chunk",
                                    "created": created,
                                    "model": model,
                                    "choices": [{
                                        "index": 0,
                                        "delta": { "content": text },
                                        "logprobs": Value::Null,
                                        "finish_reason": Value::Null
                                    }]
                                });
                                yield Ok::<Event, Infallible>(Event::default().data(chunk.to_string()));
                            }
                        }
                    }
                    Some("turn/completed") => {
                        if let Some(session_id) = session_id.clone() {
                            let _ = sessions.upsert(session_id, thread_id.clone(), model.clone()).await;
                        }
                        let wrapper = build_wrapper_metadata(
                            &stream_state,
                            &stream_request,
                            thread_id.clone(),
                            wrapper_usage_from_stream(&usage)
                        );
                        log_response_summary(
                            "chat_completions",
                            &stream_request,
                            stream_model_details.as_ref(),
                            true,
                            &thread_id,
                            &response_text,
                            wrapper.as_ref(),
                        );

                        let final_chunk = json!({
                            "id": chat_id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {},
                                "logprobs": Value::Null,
                                "finish_reason": "stop"
                            }]
                        });
                        yield Ok::<Event, Infallible>(Event::default().data(final_chunk.to_string()));
                        if include_usage {
                            let usage_chunk = json!({
                                "id": chat_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [],
                                "usage": build_openai_usage_from_stream(&usage),
                                "wrapper": wrapper
                            });
                            yield Ok::<Event, Infallible>(Event::default().data(usage_chunk.to_string()));
                        } else if stream_request.include_wrapper_metadata() {
                            let wrapper_chunk = json!({
                                "id": chat_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [],
                                "wrapper": wrapper
                            });
                            yield Ok::<Event, Infallible>(Event::default().data(wrapper_chunk.to_string()));
                        }
                        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
                        break;
                    }
                    Some("error") => {
                        let will_retry = message
                            .params
                            .as_ref()
                            .and_then(|value| value.get("willRetry"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);

                        if will_retry {
                            continue;
                        }

                        if !sent_any_text {
                            let error_text = message
                                .params
                                .as_ref()
                                .and_then(|value| value.pointer("/error/message"))
                                .and_then(Value::as_str)
                                .unwrap_or("Codex reported an error.");
                            log_response_error_summary(
                                "chat_completions",
                                &stream_request,
                                stream_model_details.as_ref(),
                                true,
                                Some(&thread_id),
                                error_text,
                                &response_text,
                            );
                            let chunk = json!({
                                "id": chat_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": { "content": format!("Codex error: {error_text}") },
                                    "logprobs": Value::Null,
                                    "finish_reason": Value::Null
                                }]
                            });
                            yield Ok::<Event, Infallible>(Event::default().data(chunk.to_string()));
                        }

                        let final_chunk = json!({
                            "id": chat_id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {},
                                "logprobs": Value::Null,
                                "finish_reason": "stop"
                            }]
                        });
                        yield Ok::<Event, Infallible>(Event::default().data(final_chunk.to_string()));
                        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
                        break;
                    }
                    _ => {}
                }
            }

            process.shutdown().await;
        };

        let sse = Sse::new(event_stream)
            .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)));
        return Ok(sse.into_response());
    }

    let exec_result = run_exec_chat(
        &state.config,
        &request,
        existing_session.as_ref(),
        &prompt_bundle,
    )
    .await?;

    if let Some(session_id) = request.session_id.clone() {
        let _ = state
            .sessions
            .upsert(
                session_id,
                exec_result.thread_id.clone(),
                request.model.clone(),
            )
            .await?;
    }

    let wrapper = build_wrapper_metadata(
        &state,
        &request,
        exec_result.thread_id.clone(),
        wrapper_usage_from_exec(&exec_result.usage),
    );
    log_response_summary(
        "chat_completions",
        &request,
        model_details.as_ref(),
        false,
        &exec_result.thread_id,
        &exec_result.text,
        wrapper.as_ref(),
    );
    let response = ChatCompletionResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: unix_timestamp(),
        model: request.model.clone(),
        choices: vec![ChatChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant".to_string(),
                content: exec_result.text,
                refusal: None,
                annotations: vec![],
            },
            logprobs: None,
            finish_reason: "stop".to_string(),
        }],
        usage: build_openai_usage(&exec_result.usage),
        service_tier: "default".to_string(),
        wrapper,
    };

    Ok(Json(response).into_response())
}

fn build_prompt_bundle(
    request: &ChatCompletionRequest,
    resumed_session: bool,
) -> AppResult<PromptBundle> {
    // Codex accepts one block of developer instructions plus one user-facing
    // prompt. This function folds Chat Completions messages into that shape.
    let mut instructions = Vec::new();
    let mut transcript = Vec::new();

    for message in &request.messages {
        let text = normalize_message_text(message);
        match message.role.as_str() {
            "system" | "developer" => {
                if !text.trim().is_empty() {
                    instructions.push(format!(
                        "{} instructions:\n{}",
                        capitalize_role(&message.role),
                        text
                    ));
                }
            }
            _ => {
                if !text.trim().is_empty() {
                    transcript.push((message.role.clone(), text));
                }
            }
        }
    }

    if transcript.is_empty() {
        return Err(AppError::BadRequest(
            "No user-visible conversation content was found in `messages`.".to_string(),
        ));
    }

    if request.enable_tools() {
        instructions.push(
            "Tool policy:\nTool-enabled mode is allowed for this request. Use Codex capabilities only when they materially improve correctness."
                .to_string(),
        );
    } else {
        instructions.push(
            "Tool policy:\nDefault to a direct answer. Do not run shell commands, modify files, or use external connectors unless the request explicitly requires tool-enabled behavior."
                .to_string(),
        );
    }

    if request.enable_web_search() {
        instructions.push(
            "Web search policy:\nLive web search is enabled for this request if Codex needs it."
                .to_string(),
        );
    } else {
        instructions
            .push("Web search policy:\nDo not browse the web for this request.".to_string());
    }

    if let Some(max_tokens) = request.effective_max_tokens() {
        instructions.push(format!(
            "Length policy:\nAim to keep the final answer within approximately {max_tokens} tokens."
        ));
    }

    let stop_sequences = request.stop_sequences();
    if !stop_sequences.is_empty() {
        instructions.push(format!(
            "Stop sequence policy:\nDo not emit any of these sequences in the final answer: {}",
            stop_sequences.join(", ")
        ));
    }

    if let Some(temperature) = request.temperature {
        let guidance = if temperature <= 0.3 {
            "Prefer a deterministic and minimally varied answer."
        } else if temperature >= 0.9 {
            "More creative variation is acceptable, but do not compromise correctness."
        } else {
            "Keep a balanced level of variation."
        };
        instructions.push(format!("Sampling policy:\n{guidance}"));
    }

    if let Some(top_p) = request.top_p {
        instructions.push(format!(
            "Sampling note:\nThe caller requested top_p={top_p:.2}. Keep output aligned with the most probable valid answer."
        ));
    }

    if let Some(user) = request.user.as_ref() {
        instructions.push(format!("Caller identity:\nOpenAI user field: {user}"));
    }

    if let Some(metadata) = request.metadata.as_ref() {
        instructions.push(format!("Request metadata:\n{}", metadata));
    }

    let prompt = if resumed_session {
        let (role, text) = transcript
            .last()
            .ok_or_else(|| AppError::BadRequest("No prompt content was found.".to_string()))?;
        instructions.push(
            "Session note:\nThis request is being attached to an existing Codex thread. Only the latest message from the incoming payload is forwarded as the new turn; prior context comes from the stored Codex session."
                .to_string(),
        );
        format!("Latest {role} message:\n{text}")
    } else {
        let rendered = transcript
            .into_iter()
            .map(|(role, text)| format!("{}:\n{}", capitalize_role(&role), text))
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("Conversation transcript:\n\n{rendered}\n\nReply as the assistant to the latest user request.")
    };

    Ok(PromptBundle {
        prompt,
        developer_instructions: if instructions.is_empty() {
            None
        } else {
            Some(instructions.join("\n\n"))
        },
    })
}

fn normalize_message_text(message: &ChatMessage) -> String {
    match &message.content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text { text } => Some(text.clone()),
                MessagePart::ImageUrl { image_url } => Some(match image_url {
                    ImageUrlPayload::Raw(url) => format!("[Image URL: {url}]"),
                    ImageUrlPayload::Object { url } => format!("[Image URL: {url}]"),
                }),
                MessagePart::Unsupported => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn render_anthropic_message_content(content: &AnthropicMessageContent) -> String {
    match content {
        AnthropicMessageContent::Text(text) => text.clone(),
        AnthropicMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                AnthropicContentBlock::Text { text } => Some(text.clone()),
                AnthropicContentBlock::Image { source } => Some(format!(
                    "[Anthropic image block: {} ({})]",
                    source
                        .media_type
                        .as_deref()
                        .unwrap_or("image payload omitted"),
                    source.source_type.as_deref().unwrap_or("unknown source")
                )),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    Some(format!("[Anthropic tool_use {name} ({id})]\n{}", input))
                }
                AnthropicContentBlock::ToolResult {
                    tool_use_id,
                    content,
                } => Some(format!(
                    "[Anthropic tool_result for {tool_use_id}]\n{}",
                    content
                )),
                AnthropicContentBlock::Unsupported => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn render_anthropic_system_prompt(system: &AnthropicSystemPrompt) -> String {
    match system {
        AnthropicSystemPrompt::Text(text) => text.clone(),
        AnthropicSystemPrompt::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                AnthropicSystemBlock::Text { text } => Some(text.clone()),
                AnthropicSystemBlock::Unsupported => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn capitalize_role(role: &str) -> String {
    let mut chars = role.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Message".to_string(),
    }
}

fn build_openai_usage(usage: &ExecUsage) -> UsageBlock {
    UsageBlock {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.input_tokens + usage.output_tokens,
        prompt_tokens_details: PromptTokensDetails {
            cached_tokens: usage.cached_input_tokens,
            audio_tokens: 0,
        },
        completion_tokens_details: CompletionTokensDetails {
            reasoning_tokens: 0,
            audio_tokens: 0,
            accepted_prediction_tokens: 0,
            rejected_prediction_tokens: 0,
        },
    }
}

fn build_openai_usage_from_stream(usage: &StreamingUsage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "prompt_tokens_details": {
            "cached_tokens": usage.cached_input_tokens,
            "audio_tokens": 0
        },
        "completion_tokens_details": {
            "reasoning_tokens": usage.reasoning_output_tokens,
            "audio_tokens": 0,
            "accepted_prediction_tokens": 0,
            "rejected_prediction_tokens": 0
        }
    })
}

fn update_streaming_usage(usage: &mut StreamingUsage, params: &Value) {
    if let Some(last) = params.get("tokenUsage").and_then(|value| value.get("last")) {
        usage.input_tokens = last
            .get("inputTokens")
            .and_then(Value::as_i64)
            .unwrap_or(usage.input_tokens);
        usage.cached_input_tokens = last
            .get("cachedInputTokens")
            .and_then(Value::as_i64)
            .unwrap_or(usage.cached_input_tokens);
        usage.output_tokens = last
            .get("outputTokens")
            .and_then(Value::as_i64)
            .unwrap_or(usage.output_tokens);
        usage.reasoning_output_tokens = last
            .get("reasoningOutputTokens")
            .and_then(Value::as_i64)
            .unwrap_or(usage.reasoning_output_tokens);
    }
}

async fn fetch_cached_models(state: &AppState) -> AppResult<Vec<CodexModel>> {
    state.model_cache.get_or_refresh(&state.config).await
}

async fn resolve_model(state: &AppState, requested: &str) -> AppResult<String> {
    let models = fetch_cached_models(state).await?;
    let trimmed = requested.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        if let Some(default_model) = models.iter().find(|candidate| candidate.is_default) {
            return Ok(default_model.model.clone());
        }
        return Ok(state.config.default_model.clone());
    }

    if let Some(candidate) = models.iter().find(|candidate| {
        candidate.model.eq_ignore_ascii_case(trimmed)
            || candidate.id.eq_ignore_ascii_case(trimmed)
            || candidate.display_name.eq_ignore_ascii_case(trimmed)
    }) {
        return Ok(candidate.model.clone());
    }

    Err(AppError::BadRequest(format!(
        "Unsupported model `{trimmed}`. Call `/v1/models` to see the available Codex-backed models."
    )))
}

fn build_output_schema(request: &ChatCompletionRequest) -> AppResult<Option<Value>> {
    let Some(format) = request.response_format.as_ref() else {
        return Ok(None);
    };

    let schema = match format {
        ResponseFormatRequest::Text => None,
        ResponseFormatRequest::JsonObject => Some(json!({ "type": "object" })),
        ResponseFormatRequest::JsonSchema { json_schema } => {
            let mut schema = json_schema.schema.clone();
            if let (Value::Object(object), Some(name)) = (&mut schema, json_schema.name.as_ref()) {
                object
                    .entry("title".to_string())
                    .or_insert_with(|| Value::String(name.clone()));
                if json_schema.strict == Some(true) {
                    object
                        .entry("additionalProperties".to_string())
                        .or_insert(Value::Bool(false));
                }
            }
            Some(schema)
        }
    };

    Ok(schema)
}

async fn materialize_output_schema_file(
    config: &AppConfig,
    request: &ChatCompletionRequest,
) -> AppResult<Option<PathBuf>> {
    let Some(schema) = build_output_schema(request)? else {
        return Ok(None);
    };

    let path = config
        .data_dir
        .join("schemas")
        .join(format!("{}.json", Uuid::new_v4()));
    fs::write(&path, serde_json::to_vec_pretty(&schema)?).await?;
    Ok(Some(path))
}

fn wrapper_usage_from_exec(usage: &ExecUsage) -> WrapperUsageMetadata {
    WrapperUsageMetadata {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: 0,
    }
}

fn wrapper_usage_from_stream(usage: &StreamingUsage) -> WrapperUsageMetadata {
    WrapperUsageMetadata {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
    }
}

fn estimate_cost_usd(
    pricing: &PricingConfig,
    model: &str,
    usage: &WrapperUsageMetadata,
) -> Option<f64> {
    let entry = pricing.models.get(model)?;
    let input_billable = (usage.input_tokens - usage.cached_input_tokens).max(0) as f64;
    let cached_input = usage.cached_input_tokens.max(0) as f64;
    let output = usage.output_tokens.max(0) as f64;
    let reasoning = usage.reasoning_output_tokens.max(0) as f64;

    let total = (input_billable / 1_000_000.0) * entry.input_per_million
        + (cached_input / 1_000_000.0)
            * entry
                .cached_input_per_million
                .unwrap_or(entry.input_per_million)
        + (output / 1_000_000.0) * entry.output_per_million
        + (reasoning / 1_000_000.0) * entry.reasoning_output_per_million.unwrap_or(0.0);

    Some((total * 1_000_000.0).round() / 1_000_000.0)
}

fn build_wrapper_metadata(
    state: &AppState,
    request: &ChatCompletionRequest,
    thread_id: String,
    usage: WrapperUsageMetadata,
) -> Option<WrapperMetadata> {
    if !request.include_wrapper_metadata() {
        return None;
    }

    let (estimated_cost_usd, pricing_source) = match state.pricing.as_ref() {
        Some(pricing) => (
            estimate_cost_usd(pricing, &request.model, &usage),
            Some(pricing.source.as_ref().clone()),
        ),
        None => (None, None),
    };

    Some(WrapperMetadata {
        thread_id,
        session_id: request.session_id.clone(),
        usage,
        estimated_cost_usd,
        pricing_source,
    })
}

async fn fetch_models(config: &AppConfig) -> AppResult<Vec<CodexModel>> {
    let mut process = AppServerProcess::start(config).await?;
    let mut cursor: Option<String> = None;
    let mut models = Vec::new();

    loop {
        let result = process
            .request(
                "model/list",
                json!({
                    "limit": 100,
                    "includeHidden": false,
                    "cursor": cursor
                }),
            )
            .await?;
        let page: CodexModelListResult = serde_json::from_value(result)?;
        models.extend(page.data);

        if let Some(next) = page.next_cursor {
            cursor = Some(next);
        } else {
            break;
        }
    }

    process.shutdown().await;
    Ok(models)
}

async fn fetch_account(config: &AppConfig) -> AppResult<CodexAccountResult> {
    let mut process = AppServerProcess::start(config).await?;
    let result = process
        .request("account/read", json!({ "refreshToken": false }))
        .await?;
    process.shutdown().await;
    Ok(serde_json::from_value(result)?)
}

async fn run_exec_chat(
    config: &AppConfig,
    request: &ChatCompletionRequest,
    existing_session: Option<&SessionRecord>,
    prompt_bundle: &PromptBundle,
) -> AppResult<ExecChatResult> {
    // Non-streaming requests use `codex exec --json` because it returns a single
    // terminal result plus usage metadata with less protocol overhead.
    let schema_path = materialize_output_schema_file(config, request).await?;
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
        "--model".to_string(),
        request.model.clone(),
        "--cd".to_string(),
        resolve_working_directory(config, request)
            .display()
            .to_string(),
        "--sandbox".to_string(),
        if request.enable_tools() {
            "workspace-write".to_string()
        } else {
            "read-only".to_string()
        },
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
        "-c".to_string(),
        format!(
            "web_search=\"{}\"",
            if request.enable_web_search() {
                "live"
            } else {
                "disabled"
            }
        ),
        "-c".to_string(),
        format!("features.apps={}", false),
        "-c".to_string(),
        format!("features.shell_tool={}", request.enable_tools()),
    ];

    if let Some(path) = schema_path.as_ref() {
        args.push("--output-schema".to_string());
        args.push(path.display().to_string());
    }

    if let Some(instructions) = &prompt_bundle.developer_instructions {
        args.push("-c".to_string());
        args.push(format!(
            "developer_instructions={}",
            toml_string(instructions)?
        ));
    }

    if let Some(effort) = &request.reasoning_effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={}", toml_string(effort)?));
    }

    if let Some(session) = existing_session {
        args.push("resume".to_string());
        args.push(session.thread_id.clone());
    }

    let mut command = codex_command(config, &args);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::Internal("failed to capture codex exec stdin".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Internal("failed to capture codex exec stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Internal("failed to capture codex exec stderr".to_string()))?;

    stdin.write_all(prompt_bundle.prompt.as_bytes()).await?;
    stdin.flush().await?;
    drop(stdin);

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buffer = String::new();
        let _ = reader.read_to_string(&mut buffer).await;
        buffer
    });

    let mut thread_id = existing_session.map(|session| session.thread_id.clone());
    let mut final_text = None;
    let mut usage = None;
    let mut fatal_error = None;
    let mut lines = BufReader::new(stdout).lines();

    // `codex exec --json` emits an event stream; extract just the assistant text,
    // usage, and fatal errors needed to build an OpenAI-style response.
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(&line)?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "thread.started" => {
                thread_id = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
            "item.completed" => {
                let completed: ExecItemCompleted = serde_json::from_value(value)?;
                match completed.item {
                    ExecItem::AgentMessage { text, .. } => final_text = Some(text),
                    ExecItem::Error { message, .. } => warn!("codex exec item error: {message}"),
                    ExecItem::Other => {}
                }
            }
            "turn.completed" => {
                let completed: ExecTurnCompleted = serde_json::from_value(value)?;
                usage = Some(completed.usage);
            }
            "turn.failed" => {
                let failed: ExecTurnFailed = serde_json::from_value(value)?;
                fatal_error = Some(failed.error.message);
            }
            "error" => {
                if let Some(message) = value.get("message").and_then(Value::as_str) {
                    warn!("codex exec event: {message}");
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await?;
    let stderr_output = stderr_task.await.unwrap_or_else(|_| String::new());

    if let Some(path) = schema_path.as_ref() {
        let _ = fs::remove_file(path).await;
    }

    if let Some(error) = fatal_error {
        return Err(AppError::Codex(error));
    }

    if !status.success() {
        let detail = if stderr_output.trim().is_empty() {
            format!("codex exec exited with status {status}")
        } else {
            stderr_output
        };
        return Err(AppError::Codex(detail));
    }

    let thread_id = thread_id
        .ok_or_else(|| AppError::Codex("codex exec did not return a thread id".to_string()))?;
    let text = final_text.unwrap_or_default();
    let usage = usage
        .ok_or_else(|| AppError::Codex("codex exec did not return usage metadata".to_string()))?;

    Ok(ExecChatResult {
        thread_id,
        text,
        usage,
    })
}

struct StreamingSession {
    process: AppServerProcess,
    thread_id: String,
}

async fn start_streaming_session(
    config: &AppConfig,
    request: &ChatCompletionRequest,
    existing_session: Option<&SessionRecord>,
    prompt_bundle: &PromptBundle,
) -> AppResult<StreamingSession> {
    // Streaming requests use app-server because it exposes live turn events that
    // can be bridged directly into OpenAI/Anthropic SSE chunks.
    let mut process = AppServerProcess::start(config).await?;
    let output_schema = build_output_schema(request)?;
    let params = json!({
        "model": request.model.clone(),
        "cwd": resolve_working_directory(config, request).display().to_string(),
        "sandbox": if request.enable_tools() { "workspace-write" } else { "read-only" },
        "approvalPolicy": "never",
        "developerInstructions": prompt_bundle.developer_instructions.clone(),
        "serviceName": config.service_name.clone(),
        "config": build_thread_config(request)
    });

    // Wrapper session continuity is implemented by resuming the prior Codex thread
    // when the caller reuses the same session_id.
    let thread_result = if let Some(session) = existing_session {
        let mut resumed = params.clone();
        resumed["threadId"] = Value::String(session.thread_id.clone());
        process.request("thread/resume", resumed).await?
    } else {
        process.request("thread/start", params).await?
    };

    let thread_id = thread_result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Codex("thread start did not return a thread id".to_string()))?
        .to_string();

    let _ = process
        .request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "cwd": resolve_working_directory(config, request).display().to_string(),
                "effort": request.reasoning_effort.clone(),
                "outputSchema": output_schema,
                "input": [
                    {
                        "type": "text",
                        "text": prompt_bundle.prompt.clone()
                    }
                ]
            }),
        )
        .await?;

    Ok(StreamingSession { process, thread_id })
}

fn build_thread_config(request: &ChatCompletionRequest) -> Value {
    // The wrapper defaults to a restrictive profile for compatibility and speed:
    // no apps, no shell tool unless explicitly requested, and no live web access
    // unless the caller opts in.
    json!({
        "web_search": if request.enable_web_search() { "live" } else { "disabled" },
        "features": {
            "apps": false,
            "shell_tool": request.enable_tools()
        },
        "sandbox_workspace_write": {
            "network_access": request.enable_web_search()
        }
    })
}

fn resolve_working_directory(config: &AppConfig, request: &ChatCompletionRequest) -> PathBuf {
    match request.working_directory.as_ref().map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => config.codex_cwd.join(path),
        None => config.codex_cwd.clone(),
    }
}

fn codex_command(config: &AppConfig, args: &[String]) -> Command {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd.exe");
        command.arg("/C").arg(&config.codex_path);
        for arg in args {
            command.arg(arg);
        }
        command
    };

    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut command = Command::new(&config.codex_path);
        for arg in args {
            command.arg(arg);
        }
        command
    };

    command.env("CODEX_HOME", &config.codex_home);
    command
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn toml_string(value: &str) -> AppResult<String> {
    serde_json::to_string(value).map_err(AppError::from)
}
