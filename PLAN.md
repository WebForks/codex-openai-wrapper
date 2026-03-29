# Codex OpenAI Wrapper Plan

## Goal

Build a Rust service that exposes OpenAI-compatible and Anthropic-compatible HTTP endpoints on top of Codex so existing SDKs can talk to a local wrapper instead of directly calling an upstream API.

The wrapper translates incoming HTTP requests into Codex CLI and Codex app-server operations, then returns API-compatible JSON or SSE responses.

## Core Design

- Runtime: Rust
- HTTP server: `axum`
- Async runtime: `tokio`
- Serialization: `serde`, `serde_json`
- Logging: `tracing`, `tracing-subscriber`
- Persistence: JSON-backed session store for the MVP
- Templating/UI: static HTML served by the Rust binary
- Codex integration:
  - `codex exec --json` for non-streaming turns and usage accounting
  - `codex app-server` JSON-RPC for models, auth status, and streaming turns

This split keeps the wrapper honest about what each Codex surface is best at:

- `codex exec` is the easiest path for non-streaming request/response plus token usage.
- `codex app-server` is the right source for `model/list`, `account/read`, and live streaming events.

## External Contract

Current API surface:

- `GET /`
- `GET /health`
- `GET /version`
- `GET /v1/models`
- `GET /v1/auth/status`
- `GET /v1/sessions`
- `GET /v1/sessions/{session_id}`
- `DELETE /v1/sessions/{session_id}`
- `POST /v1/chat/completions`
- `POST /v1/messages`

Planned next:

- stronger Anthropic field coverage
- richer parameter mapping for OpenAI compatibility
- pricing metadata
- tests and Docker packaging

## Request Flow

### OpenAI Chat Completions

1. Accept OpenAI-style chat completion JSON.
2. Validate the requested model against live Codex models from `model/list`.
3. Convert `messages` into:
   - Codex developer instructions from `system` and `developer` roles
   - Codex user turn input from the remaining transcript
4. Resolve wrapper `session_id` to a Codex `thread_id` when present.
5. For non-streaming requests, run `codex exec --json`.
6. For streaming requests, use `codex app-server` with `thread/start` or `thread/resume`, then `turn/start`.
7. Persist or update the wrapper session mapping.
8. Return OpenAI-compatible JSON or SSE.

### Anthropic Messages

1. Accept Anthropic-style `/v1/messages` JSON.
2. Normalize Anthropic `system` and `messages` fields into the same internal request shape used by chat completions.
3. Reuse the same Codex execution path.
4. Return Anthropic-compatible JSON or SSE event streams.

### Models

1. Start `codex app-server`.
2. Send `initialize` then `initialized`.
3. Call `model/list`.
4. Translate the result into OpenAI `GET /v1/models` shape.

### Auth Status

1. Start `codex app-server`.
2. Send `initialize` then `initialized`.
3. Call `account/read`.
4. Return wrapper-friendly auth diagnostics.

## Compatibility Rules

- OpenAI-compatible endpoints come first.
- Anthropic-native clients should work against `/v1/messages`.
- OpenAI-native clients should work against `/v1/chat/completions`.
- Wrapper auth should accept either `Authorization: Bearer ...` or `x-api-key`.
- Wrapper-specific extensions are accepted as top-level fields such as `session_id`, `enable_tools`, and `enable_web_search`.
- `stream: true` returns SSE in the appropriate protocol shape for each endpoint.
- Unsupported fields are accepted when safe and ignored when they do not map cleanly.
- Unsupported behavior should return clear structured errors instead of silent misbehavior.

## Safety And Performance Defaults

- Default mode is conservative.
- Default web search is disabled.
- Default approval policy is `never`.
- Default sandbox is `read-only` unless tool usage is explicitly enabled.
- Tool-heavy behavior is opt-in.

## Session Strategy

- Store wrapper session mappings in a JSON file for the MVP:
  - wrapper `session_id`
  - Codex `thread_id`
  - selected model
  - created and updated timestamps
- Codex remains the source of truth for conversation history.
- The wrapper stores routing metadata only, not the full transcript.

## API Explorer

The root landing page should include:

- service version
- health status
- auth status
- available models
- example curl and SDK snippets
- a simple browser-side form to submit a test chat completion

## Milestones

### Phase 1: MVP

- [x] Write implementation plan
- [x] Scaffold Rust service
- [x] Implement `GET /health`
- [x] Implement `GET /version`
- [x] Implement `GET /v1/models`
- [x] Implement `GET /v1/auth/status`
- [x] Implement `GET /v1/sessions`
- [x] Implement `GET /v1/sessions/{session_id}`
- [x] Implement `DELETE /v1/sessions/{session_id}`
- [x] Implement non-streaming `POST /v1/chat/completions`
- [x] Implement streaming `POST /v1/chat/completions`
- [x] Implement Anthropic-compatible `POST /v1/messages`
- [x] Add root landing page
- [x] Add JSON-backed session mapping

### Phase 2: Compatibility Expansion

- [x] Improve Anthropic field coverage beyond the initial text-only MVP
- [x] Add more OpenAI parameter mapping
- [x] Add stronger model metadata and validation
- [x] Add optional cost metadata derived from token usage

### Phase 3: Hardening

- [x] Add integration tests with mock Codex process fixtures
- [x] Add better error classification
- [x] Add rate limiting
- [x] Add wrapper auth configuration docs
- [x] Add Docker packaging

## Implementation Notes

- The project should stay honest about what is native Codex behavior versus wrapper-added behavior.
- "Real-time cost tracking" should start as computed metadata from token usage until a stable upstream cost field is available.
- The first implementation should prioritize correctness of request and response mapping over broad surface area.
- Current environment note: `cargo check` and `cargo test` now pass in this workspace. The Cargo registry required a localhost sparse-registry proxy because native Windows Schannel fetches to crates.io were failing on this machine.
