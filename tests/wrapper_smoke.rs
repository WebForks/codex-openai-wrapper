use std::{
    env, fs,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

struct TestServer {
    child: Child,
    base_url: String,
    api_key: String,
    temp_dir: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

async fn spawn_server(rate_limit_requests: u32) -> TestServer {
    spawn_server_with_defaults(rate_limit_requests, None, None).await
}

async fn spawn_server_with_default_model(
    rate_limit_requests: u32,
    default_model: Option<&str>,
) -> TestServer {
    spawn_server_with_defaults(rate_limit_requests, default_model, None).await
}

async fn spawn_server_with_defaults(
    rate_limit_requests: u32,
    default_model: Option<&str>,
    default_reasoning_effort: Option<&str>,
) -> TestServer {
    let port = free_port();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = env!("CARGO_BIN_EXE_codex-openai-wrapper");
    let fixture = manifest_dir
        .join("tests")
        .join("fixtures")
        .join("mock-codex.cmd");
    let temp_dir = env::temp_dir().join(format!(
        "codex-openai-wrapper-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(temp_dir.join("codex-home")).unwrap();
    fs::create_dir_all(temp_dir.join("data")).unwrap();
    let pricing_path = temp_dir.join("pricing.json");
    fs::write(
        &pricing_path,
        serde_json::to_vec_pretty(&json!({
            "gpt-5.4": {
                "input_per_million": 1.0,
                "cached_input_per_million": 0.1,
                "output_per_million": 2.0,
                "reasoning_output_per_million": 0.5
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut command = Command::new(binary);
    let stdout_log = temp_dir.join("wrapper.stdout.log");
    let stderr_log = temp_dir.join("wrapper.stderr.log");
    command
        .env("HOST", "127.0.0.1")
        .env("PORT", port.to_string())
        .env("CODEX_PATH", fixture)
        .env("CODEX_HOME", temp_dir.join("codex-home"))
        .env("WRAPPER_DATA_DIR", temp_dir.join("data"))
        .env("WRAPPER_PRICING_FILE", pricing_path)
        .env("WRAPPER_API_KEY", "test-key")
        .env("RATE_LIMIT_REQUESTS", rate_limit_requests.to_string())
        .env("RATE_LIMIT_WINDOW_SECS", "60")
        .env("RUST_LOG", "warn")
        .stdout(fs::File::create(&stdout_log).unwrap())
        .stderr(fs::File::create(&stderr_log).unwrap());

    if let Some(default_model) = default_model {
        command.env("DEFAULT_MODEL", default_model);
    }
    if let Some(default_reasoning_effort) = default_reasoning_effort {
        command.env("DEFAULT_REASONING_EFFORT", default_reasoning_effort);
    } else {
        command.env("DEFAULT_REASONING_EFFORT", "");
    }

    let child = command.spawn().unwrap();

    let server = TestServer {
        child,
        base_url: format!("http://127.0.0.1:{port}"),
        api_key: "test-key".to_string(),
        temp_dir,
        stdout_log,
        stderr_log,
    };

    wait_for_server(&server).await;
    server
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn wait_for_server(server: &TestServer) {
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if let Ok(response) = client.get(format!("{}/", server.base_url)).send().await {
            if response.status().is_success() {
                return;
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
    let stdout = fs::read_to_string(&server.stdout_log).unwrap_or_default();
    let stderr = fs::read_to_string(&server.stderr_log).unwrap_or_default();
    panic!("server did not become ready\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}");
}

#[tokio::test]
async fn chat_completions_returns_wrapper_metadata_and_cost() {
    let server = spawn_server(20).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "default",
            "include_wrapper_metadata": true,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": "Return JSON." },
                { "role": "user", "content": "Say hi." }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "gpt-5.4");
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("\"mock json\""));
    assert_eq!(body["wrapper"]["thread_id"], "thread-new");
    assert!(body["wrapper"]["estimated_cost_usd"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn default_model_alias_applies_to_default_requests() {
    let server = spawn_server_with_default_model(20, Some("gpt-5.4-high")).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "default",
            "messages": [
                { "role": "user", "content": "Use the wrapper default." }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "gpt-5.4");
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("effort=high"));
}

#[tokio::test]
async fn plain_default_model_still_applies_to_default_requests() {
    let server = spawn_server_with_default_model(20, Some("gpt-5.4")).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "default",
            "messages": [
                { "role": "user", "content": "Use the plain wrapper default." }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "gpt-5.4");
}

#[tokio::test]
async fn default_reasoning_effort_env_applies_to_default_requests() {
    let server = spawn_server_with_defaults(20, Some("gpt-5.4"), Some("xhigh")).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "default",
            "messages": [
                { "role": "user", "content": "Use the configured default reasoning effort." }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "gpt-5.4");
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("effort=xhigh"));
}

#[tokio::test]
async fn chat_completions_accepts_reasoning_suffix_alias() {
    let server = spawn_server(20).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "gpt-5.4-xhigh",
            "messages": [
                { "role": "user", "content": "Use careful reasoning." }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "gpt-5.4");
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("effort=xhigh"));
}

#[tokio::test]
async fn chat_completions_forwards_image_inputs_to_codex() {
    let server = spawn_server(20).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "gpt-5.4",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "Analyze the attached image." },
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": "https://example.com/sample.png",
                                "detail": "high"
                            }
                        }
                    ]
                }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("[images=1]"));
}

#[tokio::test]
async fn anthropic_messages_forward_image_inputs_to_codex() {
    let server = spawn_server(20).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/messages", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "gpt-5.4",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "Describe this image." },
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "aGVsbG8="
                            }
                        }
                    ]
                }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert!(body["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("[images=1]"));
}

#[tokio::test]
async fn conflicting_reasoning_alias_and_field_returns_bad_request() {
    let server = spawn_server(20).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "gpt-5.4-low",
            "reasoning_effort": "xhigh",
            "messages": [
                { "role": "user", "content": "This should fail." }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("conflicts with explicit reasoning_effort"));
}

#[tokio::test]
async fn anthropic_messages_streams_expected_events() {
    let server = spawn_server(20).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/messages", server.base_url))
        .header("x-api-key", &server.api_key)
        .json(&json!({
            "model": "gpt-5.4",
            "max_tokens": 256,
            "stream": true,
            "messages": [
                { "role": "user", "content": "Stream this response." }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: message_start"));
    assert!(body.contains("event: content_block_delta"));
    assert!(body.contains("streamed mock"));
    assert!(body.contains("\"wrapper\""));
}

#[tokio::test]
async fn rate_limit_triggers_after_repeated_requests() {
    let server = spawn_server(2).await;
    let client = reqwest::Client::new();

    for _ in 0..2 {
        let response = client
            .post(format!("{}/v1/chat/completions", server.base_url))
            .bearer_auth(&server.api_key)
            .json(&json!({
                "model": "gpt-5.4",
                "messages": [
                    { "role": "user", "content": "hello" }
                ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url))
        .bearer_auth(&server.api_key)
        .json(&json!({
            "model": "gpt-5.4",
            "messages": [
                { "role": "user", "content": "third request" }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get("retry-after").is_some());
}
