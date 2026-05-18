# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.95
ARG DEBIAN_VERSION=bookworm
ARG CODEX_CLI_VERSION=0.121.0

FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin symphony

FROM node:24-${DEBIAN_VERSION}-slim AS runtime
ARG CODEX_CLI_VERSION
ENV HOME=/home/symphony \
    NPM_CONFIG_AUDIT=false \
    NPM_CONFIG_FUND=false \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    RUST_LOG=info \
    SYMPHONY_WORKSPACE_ROOT=/app/.symphony-workspaces
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash ca-certificates git openssh-client tini \
    && npm install -g @openai/codex@${CODEX_CLI_VERSION} \
    && groupadd --gid 10001 symphony \
    && useradd --uid 10001 --gid symphony --create-home --home-dir /home/symphony --shell /usr/sbin/nologin symphony \
    && mkdir -p /app/.symphony-workspaces /home/symphony/.codex \
    && chown -R symphony:symphony /app /home/symphony \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /tmp/*
COPY --from=build /src/target/release/symphony /usr/local/bin/symphony
WORKDIR /app
USER symphony
VOLUME ["/app/.symphony-workspaces"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/symphony"]
