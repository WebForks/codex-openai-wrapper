# codex-openai-wrapper

Rust wrapper that exposes OpenAI-compatible and Anthropic-compatible endpoints on top of the Codex CLI.

This project is aimed at local development and SDK compatibility:

- OpenAI-compatible `POST /v1/chat/completions`
- Anthropic-compatible `POST /v1/messages`
- streaming and non-streaming responses
- live model discovery via `GET /v1/models`
- cached model validation and canonical model resolution
- auth diagnostics via `GET /v1/auth/status`
- wrapper-managed session continuity via `session_id`
- optional wrapper metadata with usage and estimated cost
- OpenAI `response_format` mapping to Codex output schemas
- configurable rate limiting
- browser landing page and API explorer at `GET /`

## Status

Implemented in code:

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

Current limitation:

- Anthropic compatibility is still text-first rather than full tool parity

## Architecture

- `codex exec --json` backs non-streaming completions and usage extraction
- `codex app-server` backs streaming, model discovery, and auth status
- wrapper session metadata is stored in `.wrapper-data/sessions.json`
- `CODEX_HOME` defaults to a repo-local `.codex-home` to avoid host-environment issues
- response-format enforcement uses Codex output schemas for both `exec` and app-server turns
- estimated cost metadata is driven by an optional pricing JSON file

## Configuration

Environment variables:

- `HOST`: bind host, default `127.0.0.1`
- `PORT`: bind port, default `8000`
- `CODEX_PATH`: Codex executable path, default `codex`
- `CODEX_HOME`: Codex home directory, default `.codex-home`
- `CODEX_WRAPPER_CWD`: default working directory forwarded to Codex, default repo root
- `WRAPPER_DATA_DIR`: wrapper state directory, default `.wrapper-data`
- `DEFAULT_MODEL`: wrapper default model used for `model: "default"` when explicitly set; supports plain ids like `gpt-5.4` and aliases like `gpt-5.4-high`
- `MODEL_CACHE_TTL_SECS`: live model cache TTL, default `30`
- `RATE_LIMIT_REQUESTS`: max requests per rate-limit window, default `120`
- `RATE_LIMIT_WINDOW_SECS`: rate-limit window length, default `60`
- `CODEX_ENABLE_RESPONSES_WEBSOCKETS`: opt in to Codex's experimental Responses WebSocket transport, default `false`
- `CODEX_ENABLE_RESPONSES_WEBSOCKETS_V2`: opt in to Codex's experimental Responses WebSocket v2 transport, default `false`
- `OPENAI_API_KEY`: optional upstream API key that Codex can inherit for direct OpenAI authentication
- `WRAPPER_API_KEY`: optional wrapper auth token
- `WRAPPER_PRICING_FILE`: optional JSON file used to estimate response cost
- `WRAPPER_SERVICE_NAME`: service name sent to Codex app-server, default `codex-openai-wrapper`

Wrapper auth behavior:

- when `WRAPPER_API_KEY` is unset, requests are accepted without wrapper-level auth enforcement
- when `WRAPPER_API_KEY` is set, the wrapper accepts either `Authorization: Bearer ...` or `x-api-key`

Model aliases:

- requests can use `model` suffixes such as `gpt-5.4-low`, `gpt-5.4-medium`, `gpt-5.4-high`, or `gpt-5.4-xhigh`
- the wrapper rewrites those aliases to the base model plus `reasoning_effort`
- if both the alias and `reasoning_effort` are supplied, they must agree
- `DEFAULT_MODEL` also supports the same alias format, for example `DEFAULT_MODEL=gpt-5.4-high`

Upstream Codex auth:

- `.env` can set values such as `CODEX_HOME=.codex-home` and `OPENAI_API_KEY=...`
- `.env` cannot run commands, so it cannot execute `codex login`
- if you keep `CODEX_HOME=.codex-home`, you must either log Codex into that home once or provide `OPENAI_API_KEY`
- `WRAPPER_API_KEY` only protects the local wrapper; it does not authenticate Codex to OpenAI

## Running

Once Rust is installed:

```powershell
$env:CODEX_HOME = ".codex-home"
$env:WRAPPER_API_KEY = "dev-token"
cargo run
```

Development mode with auto-reload:

```powershell
cargo install cargo-watch
cargo watch -x run
```

Run the test suite:

```powershell
cargo test
```

`.env` files are loaded automatically on startup. A minimal example is included in [.env.example](./.env.example), so a normal local flow is:

```powershell
Copy-Item .env.example .env
cargo run
```

If you want the repo-local `CODEX_HOME` to have persistent Codex auth, run this once in a shell:

```powershell
$env:CODEX_HOME = (Resolve-Path .codex-home).Path
codex login
```

If you prefer API-key auth, put `OPENAI_API_KEY=...` in `.env` or log in with the key once:

```powershell
$env:CODEX_HOME = (Resolve-Path .codex-home).Path
$env:OPENAI_API_KEY = "your-openai-api-key"
$env:OPENAI_API_KEY | codex login --with-api-key
```

To verify that Codex is authenticated through the wrapper:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/v1/auth/status -Headers @{
  Authorization = "Bearer dev-token"
} | ConvertTo-Json -Depth 10
```

You want to see `authenticated: true`.

If Cargo cannot reach crates.io on this Windows machine because of Schannel TLS failures, there is an optional local workaround script at `scripts/cargo_registry_proxy.py`. It is not required for normal environments and is not wired into Cargo by default.

## Usage

### What this wrapper does

The wrapper accepts OpenAI-compatible and Anthropic-compatible HTTP requests, translates them into Codex CLI or Codex app-server calls, and returns OpenAI-style JSON or SSE back to the client.

In practice that means:

- OpenAI SDKs can point `base_url` at `http://127.0.0.1:8000/v1`
- Anthropic-style clients can call `POST /v1/messages`
- Cherry Studio can use this as an `OpenAI`-type custom provider
- Codex remains the real model/runtime underneath

### Startup flow

1. Configure `.env`.
2. Make sure Codex is authenticated in the same `CODEX_HOME` the wrapper will use.
3. Start the server with `cargo run`.
4. Verify auth and models before connecting a client.

Example:

```powershell
Copy-Item .env.example .env
$env:CODEX_HOME = (Resolve-Path .codex-home).Path
codex login
cargo run
```

### Wrapper auth vs upstream Codex auth

- `WRAPPER_API_KEY` authenticates the client to this local wrapper
- `OPENAI_API_KEY` or `codex login` authenticates Codex to OpenAI upstream
- if the wrapper can be reached but Codex is unauthenticated, clients will fail with upstream `401` errors

### Check that the wrapper is healthy

Health:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/health -Headers @{
  Authorization = "Bearer dev-token"
} | ConvertTo-Json -Depth 10
```

Version:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/version -Headers @{
  Authorization = "Bearer dev-token"
} | ConvertTo-Json -Depth 10
```

Auth status:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/v1/auth/status -Headers @{
  Authorization = "Bearer dev-token"
} | ConvertTo-Json -Depth 10
```

You want `authenticated: true`.

### List available models

PowerShell:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/v1/models -Headers @{
  Authorization = "Bearer dev-token"
} | ConvertTo-Json -Depth 10
```

The `id` field is the model name clients should use. Reasoning aliases are accepted in requests, for example `gpt-5.4-xhigh`, but the base live model ids come from `/v1/models`.

If you explicitly set `DEFAULT_MODEL`, the wrapper uses that value when a client sends `model: "default"`. If you do not set `DEFAULT_MODEL`, the wrapper continues to use Codex's live upstream default model when one is available.

### OpenAI-compatible usage

OpenAI Python SDK:

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8000/v1",
    api_key="dev-token",
)

resp = client.chat.completions.create(
    model="gpt-5.4-xhigh",
    messages=[{"role": "user", "content": "Summarize this repository."}],
)

print(resp.choices[0].message.content)
```

Raw HTTP with PowerShell:

```powershell
$body = @{
  model = "gpt-5.4-xhigh"
  messages = @(
    @{ role = "user"; content = "Explain what this wrapper does." }
  )
} | ConvertTo-Json -Depth 10

Invoke-RestMethod http://127.0.0.1:8000/v1/chat/completions `
  -Method Post `
  -Headers @{
    Authorization = "Bearer dev-token"
    "Content-Type" = "application/json"
  } `
  -Body $body | ConvertTo-Json -Depth 10
```

Raw HTTP with `curl.exe`:

```powershell
curl.exe http://127.0.0.1:8000/v1/chat/completions `
  -H "content-type: application/json" `
  -H "authorization: Bearer dev-token" `
  -d "{\"model\":\"gpt-5.4-xhigh\",\"messages\":[{\"role\":\"user\",\"content\":\"Explain what this wrapper does.\"}]}"
```

### Anthropic-compatible usage

Non-streaming:

```powershell
$body = @{
  model = "gpt-5.4"
  max_tokens = 1024
  messages = @(
    @{ role = "user"; content = "Explain what this wrapper does." }
  )
} | ConvertTo-Json -Depth 10

Invoke-RestMethod http://127.0.0.1:8000/v1/messages `
  -Method Post `
  -Headers @{
    "x-api-key" = "dev-token"
    "Content-Type" = "application/json"
  } `
  -Body $body | ConvertTo-Json -Depth 10
```

### Streaming

OpenAI-style streaming:

```powershell
curl.exe -N http://127.0.0.1:8000/v1/chat/completions `
  -H "content-type: application/json" `
  -H "authorization: Bearer dev-token" `
  -d "{\"model\":\"gpt-5.4\",\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"Stream a short answer.\"}]}"
```

Anthropic-style streaming:

```powershell
curl.exe -N http://127.0.0.1:8000/v1/messages `
  -H "content-type: application/json" `
  -H "x-api-key: dev-token" `
  -d "{\"model\":\"gpt-5.4\",\"stream\":true,\"max_tokens\":256,\"messages\":[{\"role\":\"user\",\"content\":\"Stream a short answer.\"}]}"
```

### Session continuity

The wrapper only resumes Codex threads when the request includes `session_id`.

- no `session_id` means a fresh Codex thread for each request
- the same `session_id` means the wrapper reuses the stored Codex `thread_id`

Example:

```powershell
$body = @{
  model = "gpt-5.4-xhigh"
  session_id = "demo-session"
  messages = @(
    @{ role = "user"; content = "Remember that my project is called Atlas." }
  )
} | ConvertTo-Json -Depth 10

Invoke-RestMethod http://127.0.0.1:8000/v1/chat/completions `
  -Method Post `
  -Headers @{
    Authorization = "Bearer dev-token"
    "Content-Type" = "application/json"
  } `
  -Body $body | ConvertTo-Json -Depth 10
```

### Session management endpoints

List sessions:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/v1/sessions -Headers @{
  Authorization = "Bearer dev-token"
} | ConvertTo-Json -Depth 10
```

Read one session:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/v1/sessions/demo-session -Headers @{
  Authorization = "Bearer dev-token"
} | ConvertTo-Json -Depth 10
```

Delete one session mapping:

```powershell
Invoke-RestMethod http://127.0.0.1:8000/v1/sessions/demo-session `
  -Method Delete `
  -Headers @{
    Authorization = "Bearer dev-token"
  } | ConvertTo-Json -Depth 10
```

Deleting the wrapper session mapping does not delete Codex's own stored thread history. It only removes the wrapper's routing entry.

### Reasoning aliases

The wrapper accepts model aliases like:

- `gpt-5.4-low`
- `gpt-5.4-medium`
- `gpt-5.4-high`
- `gpt-5.4-xhigh`

Those are translated into:

- base model id, for example `gpt-5.4`
- `reasoning_effort`, for example `xhigh`

This is useful for clients that only expose a model picker and do not let you attach arbitrary extra JSON fields.

### Structured output with `response_format`

OpenAI-style request with schema-constrained JSON output:

```powershell
$body = @{
  model = "gpt-5.4"
  include_wrapper_metadata = $true
  response_format = @{
    type = "json_schema"
    json_schema = @{
      name = "summary"
      strict = $true
      schema = @{
        type = "object"
        properties = @{
          summary = @{ type = "string" }
        }
        required = @("summary")
      }
    }
  }
  messages = @(
    @{ role = "user"; content = "Describe this project in one sentence." }
  )
} | ConvertTo-Json -Depth 20

Invoke-RestMethod http://127.0.0.1:8000/v1/chat/completions `
  -Method Post `
  -Headers @{
    Authorization = "Bearer dev-token"
    "Content-Type" = "application/json"
  } `
  -Body $body | ConvertTo-Json -Depth 20
```

### Tool usage and web search

The wrapper is restrictive by default:

- tools disabled unless `enable_tools` is set or OpenAI tool fields are present
- live web search disabled unless `enable_web_search` is set

Example:

```powershell
$body = @{
  model = "gpt-5.4"
  enable_tools = $true
  enable_web_search = $true
  messages = @(
    @{ role = "user"; content = "Inspect this repo and summarize the main Rust modules." }
  )
} | ConvertTo-Json -Depth 10

Invoke-RestMethod http://127.0.0.1:8000/v1/chat/completions `
  -Method Post `
  -Headers @{
    Authorization = "Bearer dev-token"
    "Content-Type" = "application/json"
  } `
  -Body $body | ConvertTo-Json -Depth 10
```

### Wrapper metadata and pricing

If `include_wrapper_metadata` is enabled, the response can include:

- `thread_id`
- `session_id`
- token usage
- estimated cost when `WRAPPER_PRICING_FILE` is configured

Pricing file example:

```json
{
  "gpt-5.4": {
    "input_per_million": 1.0,
    "cached_input_per_million": 0.1,
    "output_per_million": 2.0,
    "reasoning_output_per_million": 0.5
  }
}
```

### Logs

When the wrapper is running, it emits structured JSON logs for:

- `wrapper_request`
- `wrapper_response`
- `wrapper_response_error`

Those logs include:

- incoming user/assistant messages
- the generated Codex prompt and developer instructions
- requested and resolved model ids
- resolved `reasoning_effort`
- model metadata from Codex
- session and thread ids
- final assistant text

### Cherry Studio

Cherry Studio should be configured as an `OpenAI`-type custom provider:

- Base URL: `http://127.0.0.1:8000/`
- API key: your `WRAPPER_API_KEY`
- Model: one of the wrapper-exposed model ids, for example `gpt-5.4` or the alias `gpt-5.4-xhigh`

If Cherry does not send `session_id`, the wrapper will start a new Codex thread per request even if Cherry includes prior visible chat history in the request payload.

## Docker

The container image is intended for running the wrapper against a mounted workspace. By default it:

- listens on `0.0.0.0:8000`
- uses `/workspace` as `CODEX_WRAPPER_CWD`
- uses `/var/lib/codex` as `CODEX_HOME`
- uses `/var/lib/codex-wrapper` for wrapper session/state files

Build:

```bash
docker build -t codex-openai-wrapper .
```

The Dockerfile pins the bundled Codex CLI via `CODEX_NPM_VERSION`. The current default is `0.116.0`, and you can override it at build time:

```bash
docker build --build-arg CODEX_NPM_VERSION=0.116.0 -t codex-openai-wrapper .
```

Recommended run with an API key:

```bash
docker run --rm -p 8000:8000 \
  -e WRAPPER_API_KEY=dev-token \
  -e OPENAI_API_KEY=your-openai-api-key \
  -e DEFAULT_MODEL=gpt-5.4-high \
  -v "$PWD:/workspace" \
  -v codex-home:/var/lib/codex \
  -v codex-wrapper-data:/var/lib/codex-wrapper \
  codex-openai-wrapper
```

PowerShell equivalent:

```powershell
docker run --rm -p 8000:8000 `
  -e WRAPPER_API_KEY=dev-token `
  -e OPENAI_API_KEY=your-openai-api-key `
  -e DEFAULT_MODEL=gpt-5.4-high `
  -v "${PWD}:/workspace" `
  -v codex-home:/var/lib/codex `
  -v codex-wrapper-data:/var/lib/codex-wrapper `
  codex-openai-wrapper
```

Why the mounts matter:

- mount your project to `/workspace` so Codex sees the real repo instead of an empty container directory
- mount `codex-home` if you want Codex auth and its own state to persist across container restarts
- mount `codex-wrapper-data` if you want wrapper session mappings to persist across container restarts

### Docker auth

Do not run `codex login` in the Dockerfile itself.

- `docker build` layers are the wrong place for secrets
- `codex login` is interactive for ChatGPT-managed auth
- baking auth into an image makes the image unsafe to share

There are two sane Docker auth paths:

1. `OPENAI_API_KEY` at `docker run` time
2. a persistent `CODEX_HOME` volume plus a one-time `codex login`

The API-key path is the simplest and is the recommended default for containers. With `OPENAI_API_KEY` set at runtime, you do not need to run `codex login` separately.

If you want persistent Codex CLI auth instead, run a one-time login against the same image and the same `CODEX_HOME` volume, then start the wrapper container normally.

For Docker specifically, `OPENAI_API_KEY` is usually the better choice. ChatGPT-managed login from inside a container can be awkward because it depends on an interactive browser/OAuth flow.

One-time login with the same image:

```bash
docker run --rm -it \
  --entrypoint codex \
  -e CODEX_HOME=/var/lib/codex \
  -v codex-home:/var/lib/codex \
  codex-openai-wrapper login
```

PowerShell equivalent:

```powershell
docker run --rm -it `
  --entrypoint codex `
  -e CODEX_HOME=/var/lib/codex `
  -v codex-home:/var/lib/codex `
  codex-openai-wrapper login
```

After that, start the wrapper with the same `codex-home` volume mounted.

If you prefer API-key login but still want Codex to persist its stored credentials inside the container volume, you can also do a one-time API-key login:

```bash
docker run --rm -it \
  --entrypoint /bin/bash \
  -e CODEX_HOME=/var/lib/codex \
  -e OPENAI_API_KEY=your-openai-api-key \
  -v codex-home:/var/lib/codex \
  codex-openai-wrapper \
  -lc 'printf "%s" "$OPENAI_API_KEY" | codex login --with-api-key'
```

### Docker notes

- the image runs as the non-root `node` user
- if your mounted workspace needs a specific UID/GID mapping, pass `--user` when running the container
- if you do not mount `/workspace`, Codex will run against the container's empty working directory
- `WRAPPER_API_KEY` only protects the wrapper; upstream Codex auth still comes from `OPENAI_API_KEY` or `codex login`

## Notes

- model validation is live and based on Codex `model/list`
- tool usage is disabled by default and must be explicitly enabled per request
- the wrapper exposes extra metadata only when requested or when using the Anthropic endpoint defaults
- the checked-in `schema/` folder is a development/reference snapshot of Codex app-server JSON schemas; the wrapper does not read it at runtime
- runtime request-specific output schemas are written under `.wrapper-data/schemas`
- integration coverage uses a mock Codex fixture through `cargo test`
- `scripts/cargo_registry_proxy.py` is an optional local build helper for Windows Cargo TLS issues; it is not part of the runtime wrapper
- Anthropic compatibility is text-first right now; richer tool and multimodal parity is a follow-up item
- see [PLAN.md](./PLAN.md) for the implementation plan and remaining work
