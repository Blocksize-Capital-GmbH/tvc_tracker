# ===============================
# Stage 1: Build
# ===============================
FROM rust:1.82-bookworm AS builder

WORKDIR /app

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./

# Create dummy src for dependency caching
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "pub mod config; pub mod rpc; pub mod metrics; pub mod poller; pub mod logging;" > src/lib.rs && \
    mkdir -p src/rpc && \
    echo "pub mod client; pub mod types;" > src/rpc/mod.rs && \
    touch src/config.rs src/metrics.rs src/poller.rs src/logging.rs src/rpc/client.rs src/rpc/types.rs

# Build dependencies only (cached layer)
RUN cargo build --release 2>/dev/null || true

# Copy actual source code
COPY src ./src

# Touch files to invalidate cache and rebuild
RUN touch src/main.rs && \
    cargo build --release

# ===============================
# Stage 2: Runtime
# ===============================
FROM debian:bookworm-slim AS runtime

# Install CA certificates for TLS
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 -s /bin/bash appuser

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/tvc_tracker /app/tvc_tracker

# Create logs directory
RUN mkdir -p /app/logs && chown -R appuser:appuser /app

USER appuser

# Expose metrics port
EXPOSE 7999

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:7999/metrics || exit 1

ENTRYPOINT ["/app/tvc_tracker"]
