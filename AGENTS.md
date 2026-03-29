# Repository Guidelines

## Project Structure & Module Organization
`src/main.rs` contains the Axum server, config loading, route handlers, rate limiting, session storage, and Codex process integration. Keep related helpers close to the handler they support until the crate is split into modules. Integration coverage lives in `tests/wrapper_smoke.rs`, backed by `tests/fixtures/mock-codex.cmd` and `tests/fixtures/mock_codex.py`. Use `scripts/cargo_registry_proxy.py` only as a local Windows Cargo workaround, not runtime code. Treat `.codex-home/` and `.wrapper-data/` as local state, and do not rely on `schema/` at runtime.

## Build, Test, and Development Commands
`cargo run` starts the wrapper and auto-loads `.env`. `cargo test` runs the integration suite against the mock Codex fixture. `cargo fmt` applies standard Rust formatting before review. `cargo watch -x run` is the preferred reload loop after `cargo install cargo-watch`. For container work, use `docker build -t codex-openai-wrapper .` and run the image with the environment variables documented in `README.md`.

## Coding Style & Naming Conventions
Target Rust 2021 and default `rustfmt` output: 4-space indentation, grouped imports, and trailing commas where the formatter keeps them. Use `snake_case` for functions and tests, `PascalCase` for structs and enums, and `SCREAMING_SNAKE_CASE` for constants. Prefer small helpers and explicit request/response mapping over dense inline logic inside handlers. Add comments only where Codex subprocess behavior or protocol translation is not obvious from the code.

## Testing Guidelines
Add or extend integration tests in `tests/wrapper_smoke.rs` for new endpoints, auth changes, rate limiting, session behavior, and model translation rules. Name tests after observable behavior, for example `chat_completions_accepts_reasoning_suffix_alias`. Keep tests deterministic by using the mock fixture instead of live Codex or real API credentials.

## Commit & Pull Request Guidelines
Git history is minimal and currently uses short imperative subjects such as `Initial commit`; continue that pattern and keep subjects concise. Pull requests should describe the behavior change, note any new or changed environment variables, and mention verification steps such as `cargo test` or a manual `Invoke-RestMethod` check. Include request or response examples when changing HTTP contract behavior.

## Security & Configuration Tips
Start from `.env.example` and keep real secrets in local `.env` only. Never commit API keys, `.wrapper-data/`, `target/`, or other generated state. During local development, prefer repo-local `CODEX_HOME` and `WRAPPER_DATA_DIR` so test and auth state stays isolated.
