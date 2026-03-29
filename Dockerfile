FROM rust:1.94-bookworm AS build

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release

COPY src ./src
RUN cargo build --release

FROM node:22-bookworm-slim

ARG CODEX_NPM_VERSION=0.116.0

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git ripgrep tini \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g "@openai/codex@${CODEX_NPM_VERSION}" \
    && npm cache clean --force

COPY --from=build /build/target/release/codex-openai-wrapper /usr/local/bin/codex-openai-wrapper

RUN mkdir -p /workspace /var/lib/codex /var/lib/codex-wrapper \
    && chown -R node:node /workspace /var/lib/codex /var/lib/codex-wrapper

WORKDIR /workspace

ENV HOST=0.0.0.0
ENV PORT=8000
ENV CODEX_PATH=codex
ENV CODEX_HOME=/var/lib/codex
ENV CODEX_WRAPPER_CWD=/workspace
ENV WRAPPER_DATA_DIR=/var/lib/codex-wrapper

USER node

EXPOSE 8000

ENTRYPOINT ["/usr/bin/tini", "--", "codex-openai-wrapper"]
