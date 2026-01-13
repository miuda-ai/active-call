# Active Call

`active-call` is a standalone Rust crate designed for building AI Voice Agents. It was originally decoupled from [rustpbx](https://github.com/restsend/rustpbx) to provide a dedicated, high-performance library for voice agent integration.

## Overview

This crate handles the complex low-level details of voice communication, making it easy to connect LLM/NLP engines with telephony platforms. It manages:

- **ASR (Automatic Speech Recognition)**: Handling speech-to-text transitions and events.
- **TTS (Text-to-Speech)**: Managing real-time audio synthesis and playback.
- **SIP & WebRTC**: Bridging traditional telephony and modern web communication protocols.
- **Voice Details**: Managing audio buffers, codecs (like Opus), and real-time streaming.

By abstracting these technical hurdles, `active-call` allows developers to focus on building intelligent dialogue systems rather than worrying about the underlying voice infrastructure.

## Key Features

- **Standalone Crate**: Decoupled from the main PBX logic for better modularity.
- **LLM/NLP Friendly**: Designed to easily feed ASR results into LLMs and stream TTS responses back to the caller.
- **Multi-Protocol Support**: Supports SIP, WebRTC, and raw WebSocket audio streams.
- **Real-time Performance**: Built with Rust for low-latency audio processing.

## Playbook demo
![Playbook demo](./docs/playbook.png)

## VAD Performance

Benchmarked on 60 seconds of 16kHz audio (Release mode):

| VAD Engine      | Implementation   | Time (60s) | RTF (Ratio) | Note                   |
| --------------- | ---------------- | ---------- | ----------- | ---------------------- |
| **TinySilero**  | Rust (Optimized) | ~60.0 ms   | 0.0010      | >2.5x faster than ONNX |
| **ONNX Silero** | ONNX Runtime     | ~158.3 ms  | 0.0026      | Standard baseline      |
| **WebRTC VAD**  | C/C++ (Bind)     | ~3.1 ms    | 0.00005     | Legacy, less accurate  |

## API Documentation

For detailed information on REST endpoints and WebSocket protocols, please refer to the [API Documentation](docs/api.md).

## Docker Usage

### Pull the Image

```bash
docker pull ghcr.io/restsend/active-call:latest
```

### Configuration

Copy the example config and customize it:

```bash
cp active-call.example.toml config.toml
```

### Run with Docker

```bash
docker run -d \
  --name active-call \
  -p 8080:8080 \
  -p 13050:13050/udp \
  -v $(pwd)/config.toml:/app/config.toml:ro \
  -v $(pwd)/config:/app/config \
  ghcr.io/restsend/active-call:latest
```

### Environment Variables

If you have API keys, save them in an `.env` file:

```bash
TENCENT_APPID=your_app_id
TENCENT_SECRET_ID=your_secret_id
TENCENT_SECRET_KEY=your_secret_key
DASHSCOPE_API_KEY=your_dashscope_api_key
```

And mount it in the container like :

```bash
  -v $(pwd)/.env:/app/.env \
```

### Port Range

Use small range `20000-20100` for local development, bigger range like `20000-30000`, or host network for production.

## License

This project is licensed under the MIT License.
