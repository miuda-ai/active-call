# ============================================
# Runtime Image (Packaging only - No Compilation)
# ============================================
FROM debian:bookworm-slim

LABEL maintainer="shenjindi@miuda.ai"
LABEL org.opencontainers.image.source="https://github.com/miuda-ai/active-call"
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
    curl \
    wget \
    unzip \
    && rm -rf /var/lib/apt/lists/*

# Download and install ONNX Runtime based on architecture
ARG TARGETARCH
ENV ORT_VERSION=1.23.2

# The microsoft/onnxruntime GitHub releases are no longer available, so use the
# official NuGet package, which ships the same Linux libraries.
RUN if [ "$TARGETARCH" = "amd64" ]; then \
        ORT_ARCH="x64"; \
    elif [ "$TARGETARCH" = "arm64" ]; then \
        ORT_ARCH="arm64"; \
    else \
        echo "Unsupported architecture: $TARGETARCH" && exit 1; \
    fi && \
    wget -q -O ort.nupkg https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ORT_VERSION}/microsoft.ml.onnxruntime.${ORT_VERSION}.nupkg && \
    unzip -j ort.nupkg "runtimes/linux-${ORT_ARCH}/native/*.so" -d /usr/local/lib/ && \
    rm ort.nupkg && \
    mv /usr/local/lib/libonnxruntime.so /usr/local/lib/libonnxruntime.so.${ORT_VERSION} && \
    ln -s libonnxruntime.so.${ORT_VERSION} /usr/local/lib/libonnxruntime.so.1 && \
    ln -s libonnxruntime.so.1 /usr/local/lib/libonnxruntime.so && \
    ldconfig

# Set ONNX Runtime dynamic library path
ENV ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so.${ORT_VERSION}

# Create application directory structure
WORKDIR /app
RUN mkdir -p /app/config/mediacache /app/config/cdr /app/config/recorders  /app/config/playbook /app/static

# Automatically pick the correct binary based on the architecture being built
# We expect binaries to be placed in bin/amd64/ and bin/arm64/ by the build script
ARG TARGETARCH
COPY --chmod=0755 bin/${TARGETARCH}/active-call /app/active-call
COPY ./static /app/static
COPY ./features /app/features
COPY ./config/playbook/hello.md /app/config/playbook/hello.md

# Expose ports
EXPOSE 8080
EXPOSE 13050/udp

# Default entrypoint
ENTRYPOINT ["/app/active-call"]

# Default command
CMD ["--conf", "/app/config.toml"]
