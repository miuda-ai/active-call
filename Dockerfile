# ============================================
# Runtime Image (Packaging only - No Compilation)
# ============================================
FROM debian:bookworm-slim

LABEL maintainer="shenjindi@miuda.ai"
LABEL org.opencontainers.image.source="https://github.com/restsend/active-call"
LABEL org.opencontainers.image.description="A SIP/WebRTC voice agent"

# Set environment variables
ARG DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV TZ=UTC

# Install runtime dependencies
RUN --mount=type=cache,target=/var/cache/apt \
    apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tzdata \
    libopus0 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create app user for security
RUN groupadd -r activeuser && useradd -r -g activeuser activeuser

# Create application directory structure
WORKDIR /app
RUN mkdir -p /app/config/mediacache /app/config/cdr /app/config/recorders /app/static

# Automatically pick the correct binary based on the architecture being built
# We expect binaries to be placed in bin/amd64/ and bin/arm64/ by the build script
ARG TARGETARCH
COPY bin/${TARGETARCH}/active-call /app/active-call
COPY ./static /app/static

# Set ownership
RUN chown -R activeuser:activeuser /app

# Switch to non-root user
USER activeuser

# Expose ports
EXPOSE 8080
EXPOSE 13050/udp
EXPOSE 20000-30000/udp

# Default entrypoint
ENTRYPOINT ["/app/active-call"]

# Default command
CMD ["--conf", "/app/config.toml"]
