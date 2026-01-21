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
    wget \
    && rm -rf /var/lib/apt/lists/*

# Download and install ONNX Runtime based on architecture
ARG TARGETARCH
ENV ORT_VERSION=1.23.2

RUN if [ "$TARGETARCH" = "amd64" ]; then \
        ORT_ARCH="x64"; \
    elif [ "$TARGETARCH" = "arm64" ]; then \
        ORT_ARCH="aarch64"; \
    else \
        echo "Unsupported architecture: $TARGETARCH" && exit 1; \
    fi && \
    wget -q https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}.tgz && \
    tar -xzf onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}.tgz && \
    mv onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}/lib/* /usr/local/lib/ && \
    rm -rf onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}* && \
    ldconfig

# Set ONNX Runtime dynamic library path
ENV ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so.${ORT_VERSION}

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
COPY ./features /app/features

# Set ownership
RUN chown -R activeuser:activeuser /app

# Switch to non-root user
USER activeuser

# Expose ports
EXPOSE 8080
EXPOSE 13050/udp

# Default entrypoint
ENTRYPOINT ["/app/active-call"]

# Default command
CMD ["--conf", "/app/config.toml"]
