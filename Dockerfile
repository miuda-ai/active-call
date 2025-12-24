# ============================================
# Stage 1: Build the active-call binary
# ============================================
FROM rust:bookworm AS rust-builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    libasound2-dev \
    libopus-dev \
    cmake \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Create build directory
WORKDIR /build

# Copy the source code
COPY Cargo.toml ./
COPY src ./src
COPY static ./static

# Build the release binary with cargo caching
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target/release/incremental \
    --mount=type=cache,target=/build/target/release/build \
    cargo build --release

# ============================================
# Stage 2: Create the runtime image
# ============================================
FROM debian:bookworm-slim

LABEL maintainer="shenjindi@miuda.ai"
LABEL org.opencontainers.image.source="https://github.com/restsend/active-call"
LABEL org.opencontainers.image.description="A SIP/WebRTC voice agent"

# Install runtime dependencies
RUN --mount=type=cache,target=/var/cache/apt \
    apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tzdata \
    libopus0 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Set environment variables
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV TZ=UTC

# Create app user for security
RUN groupadd -r activeuser && useradd -r -g activeuser activeuser

# Create application directory structure
WORKDIR /app
RUN mkdir -p /app/config/mediacache /app/config/cdr /app/config/recorders /app/static \
    && chown -R activeuser:activeuser /app

# Copy built binary and static files
COPY --from=rust-builder /build/target/release/active-call /app/active-call
COPY --from=rust-builder /build/static /app/static

# Set ownership
RUN chown -R activeuser:activeuser /app

# Switch to non-root user
USER activeuser

# Expose ports
# - HTTP API port
EXPOSE 8080
# - SIP UDP port (default)
EXPOSE 13050/udp
# - RTP port range (customize based on rtp_start_port/rtp_end_port in config)
EXPOSE 20000-30000/udp

# Default entrypoint
ENTRYPOINT ["/app/active-call"]

# Default command (can be overridden)
CMD ["--conf", "/app/config.toml"]
