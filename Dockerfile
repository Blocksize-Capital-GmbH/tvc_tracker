# ===============================
# Stage 1: Build
# ===============================
FROM rust:1.85-bookworm AS builder

WORKDIR /app

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./

# Create a dummy project to cache dependencies
RUN mkdir src && \
    echo "fn main() { println!(\"placeholder\"); }" > src/main.rs

# Build dependencies only (this layer is cached if Cargo.toml/Cargo.lock don't change)
RUN cargo build --release && \
    rm -rf src target/release/deps/tvc_tracker* target/release/tvc_tracker*

# Copy actual source code
COPY src ./src

# Build the real application
RUN cargo build --release

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
