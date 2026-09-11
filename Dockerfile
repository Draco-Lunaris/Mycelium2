# syntax=docker/dockerfile:1

# ---------- Dependency cook stage (cargo-chef) ----------
FROM lukemathwalker/cargo-chef:latest-rust-1.88 AS chef
WORKDIR /app

# ---------- Plan stage ----------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---------- Dependency build stage ----------
FROM chef AS builder
# openidconnect → reqwest 0.12 default-features pulls native-tls →
# OpenSSL headers + pkg-config required.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies only — this layer caches until Cargo.lock changes.
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
ENV SQLX_OFFLINE=true
RUN cargo build --release --bin mycelium2 --bin mycelium2-cli

# ---------- Runtime stage ----------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 mycelium \
    && useradd --uid 1000 --gid mycelium --create-home --shell /usr/sbin/nologin mycelium

# Data directory (volume mount point). Writable by the runtime user.
RUN mkdir -p /opt/mycelium2/data && chown -R mycelium:mycelium /opt/mycelium2

COPY --from=builder --chown=mycelium:mycelium \
    /app/target/release/mycelium2 \
    /app/target/release/mycelium2-cli \
    /usr/local/bin/

USER mycelium
WORKDIR /opt/mycelium2
VOLUME ["/opt/mycelium2/data"]
EXPOSE 443 80

ENV MYCELIUM2_DATA_DIR=/opt/mycelium2/data \
    MYCELIUM2_HTTPS_ADDR=0.0.0.0:443 \
    MYCELIUM2_HTTP_ADDR=0.0.0.0:80

# Container-native: run in the foreground; SIGTERM triggers graceful
# shutdown (the binary listens for SIGTERM/SIGINT).
STOPSIGNAL SIGTERM

# Health: the HTTP redirect listener answers on / (302 → https).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:80/ >/dev/null || exit 1

CMD ["mycelium2"]