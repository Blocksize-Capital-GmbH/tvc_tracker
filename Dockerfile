# ===============================
# Stage 1: Chef (dependency caching)
# ===============================
FROM --platform=$BUILDPLATFORM rust:1.85-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# ===============================
# Stage 2: Planner (analyze dependencies)
# ===============================
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ===============================
# Stage 3: Builder (compile with cached dependencies)
# ===============================
FROM chef AS builder

ARG TARGETPLATFORM

# Install cross-compilation tools for ARM64
RUN if [ "$TARGETPLATFORM" = "linux/arm64" ]; then \
        apt-get update && \
        apt-get install -y gcc-aarch64-linux-gnu && \
        rustup target add aarch64-unknown-linux-gnu; \
    fi

COPY --from=planner /app/recipe.json recipe.json

# Build dependencies (this layer is heavily cached)
RUN if [ "$TARGETPLATFORM" = "linux/arm64" ]; then \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
        cargo chef cook --release --target aarch64-unknown-linux-gnu --recipe-path recipe.json; \
    else \
        cargo chef cook --release --recipe-path recipe.json; \
    fi

# Copy source and build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN if [ "$TARGETPLATFORM" = "linux/arm64" ]; then \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
        cargo build --release --target aarch64-unknown-linux-gnu && \
        cp target/aarch64-unknown-linux-gnu/release/tvc_tracker /app/tvc_tracker; \
    else \
        cargo build --release && \
        cp target/release/tvc_tracker /app/tvc_tracker; \
    fi

# ===============================
# Stage 4: Runtime
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
COPY --from=builder /app/tvc_tracker /app/tvc_tracker

# Create logs directory
RUN mkdir -p /app/logs && chown -R appuser:appuser /app

USER appuser

# Expose metrics port
EXPOSE 7999

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:7999/metrics || exit 1

ENTRYPOINT ["/app/tvc_tracker"]
