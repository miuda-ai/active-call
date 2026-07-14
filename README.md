# Active Call

[![Crates.io](https://img.shields.io/crates/v/active-call.svg)](https://crates.io/crates/active-call)
[![Downloads](https://img.shields.io/crates/d/active-call.svg)](https://crates.io/crates/active-call)
[![Commits](https://img.shields.io/github/commit-activity/m/miuda-ai/active-call)](https://github.com/miuda-ai/active-call/commits/main)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

`active-call` is a standalone Rust crate for building AI Voice Agents. It provides high-performance infrastructure bridging AI models with real-world telephony and web communications.

📖 **Documentation** → [English](./docs/en/README.md) | [中文](./docs/zh/README.md) | [API Reference](./docs/api.md)

## Key Capabilities

### 1. Multi-Protocol Audio Gateway

- **SIP (Telephony)**: UDP, TCP, TLS (SIPS), WebSocket. Register as extension to FreeSWITCH / Asterisk / [RustPBX](https://github.com/restsend/rustpbx), or handle direct SIP calls. PSTN via [Twilio](./docs/twilio_integration.md) and [Telnyx](./docs/telnyx_integration.md).
- **WebRTC**: Browser-to-agent SRTP. *(Requires HTTPS or 127.0.0.1)*
- **Voice over WebSocket**: Push raw PCM/encoded audio, receive real-time events.

### 2. Dual-Engine Dialogue

- **Traditional Pipeline**: VAD → ASR → LLM → TTS. Supports OpenAI, Aliyun, Azure, Tencent and more.
- **Realtime Streaming**: Native OpenAI/Azure Realtime API — full-duplex, ultra-low latency.

### 3. Playbook — Stateful Voice Agents

Define personas, scenes, and flows in Markdown files:

```markdown
---
asr:
  provider: "sensevoice"
tts:
  provider: "supertonic"
  speaker: "F1"
llm:
  provider: "openai"
  model: "${OPENAI_MODEL}"
  apiKey: "${OPENAI_API_KEY}"
  features: ["intent_clarification", "emotion_resonance"]
dtmf:
  "0": { action: "hangup" }
posthook:
  url: "https://api.example.com/webhook"
  summary: "detailed"
---

# Scene: greeting
<dtmf digit="1" action="goto" scene="tech_support" />

You are a friendly AI for {{ company_name }}. Greet the caller warmly.

# Scene: tech_support
How can I help with your system? I can transfer you: <refer to="sip:human@domain.com" />
```

> 💡 `${VAR}` = environment variables (config-time). `{{var}}` = runtime variables (per-call).

### 4. Offline AI (Privacy-First)

Run ASR and TTS locally — no cloud API required:

- **Offline ASR**: [SenseVoice](https://github.com/FunAudioLLM/SenseVoice) — zh, en, ja, ko, yue
- **Offline TTS**: [Supertonic](https://github.com/supertone-inc/supertonic) — en, ko, es, pt, fr

```bash
# Download models
docker run --rm -v $(pwd)/data/models:/models \
  ghcr.io/miuda-ai/active-call:latest \
  --download-models all --models-dir /models --exit-after-download

# Run with offline models
docker run -d --net host \
  -v $(pwd)/data/models:/app/models \
  -v $(pwd)/config:/app/config \
  ghcr.io/miuda-ai/active-call:latest
```

> **Mainland China**: Add `-e HF_ENDPOINT=https://hf-mirror.com` to use the HuggingFace mirror.

### 5. High-Performance Media Core

| VAD Engine      | Time (60s audio) | RTF    | Note              |
| --------------- | ---------------- | ------ | ----------------- |
| **TinySilero**  | ~60 ms           | 0.0010 | >2.5× faster ONNX |
| **ONNX Silero** | ~158 ms          | 0.0026 | Standard baseline |
| **WebRTC VAD**  | ~3 ms            | 0.00005| Legacy            |

Codec support: PCM16, G.711 (PCMU/PCMA), G.722, Opus.

## Quick Start

```bash
# Webhook handler
./active-call --handler https://example.com/webhook

# Playbook handler
./active-call --handler config/playbook/greeting.md

# Outbound SIP call
./active-call --call sip:1001@127.0.0.1:5060 --handler greeting.md

# With external IP and codecs
./active-call --handler default.md --external-ip 1.2.3.4 --codecs pcmu,pcma,opus
```

### Docker

```bash
docker run -d --net host \
  --name active-call \
  -v $(pwd)/config.toml:/app/config.toml:ro \
  -v $(pwd)/config:/app/config \
  ghcr.io/miuda-ai/active-call:latest
```

### Playbook Handler Routing

```toml
[handler]
type = "playbook"
default = "greeting.md"

[[handler.rules]]
caller = "^\\+1\\d{10}$"
callee = "^sip:support@.*"
playbook = "support.md"

[[handler.rules]]
caller = "^\\+86\\d+"
playbook = "chinese.md"
```

## SIP Carrier Integration

### TLS + SRTP (Required by Twilio)

```toml
tls_port      = 5061
tls_cert_file = "./certs/cert.pem"
tls_key_file  = "./certs/key.pem"
enable_srtp   = true
```

- [Twilio Elastic SIP Trunking Guide](./docs/twilio_integration.md)
- [Telnyx SIP Trunking Guide](./docs/telnyx_integration.md)

## Building from Source

```bash
# rustc may require extra stack when compiling certain crates
export RUST_MIN_STACK=16777216
cargo build --release
```

Or add to your shell profile:
```bash
echo 'export RUST_MIN_STACK=16777216' >> ~/.bashrc
```

## Environment Variables

```bash
# OpenAI / Azure
OPENAI_API_KEY=sk-...
AZURE_OPENAI_API_KEY=...
AZURE_OPENAI_ENDPOINT=https://your-resource.openai.azure.com/

# Aliyun DashScope
DASHSCOPE_API_KEY=sk-...

# Tencent Cloud
TENCENT_APPID=...
TENCENT_SECRET_ID=...
TENCENT_SECRET_KEY=...

# Offline models
OFFLINE_MODELS_DIR=/path/to/models
```

## Demo

![Playbook demo](./docs/playbook.png)

## SDKs

- **Go**: [rustpbxgo](https://github.com/restsend/rustpbxgo) — Official Go client

## Documentation

| Language | Links |
|----------|-------|
| **English** | [Docs Hub](./docs/en/README.md) · [API Reference](./docs/api.md) · [Config Guide](./docs/en/config_guide.md) · [Playbook Tutorial](./docs/en/playbook_tutorial.md) · [Advanced Features](./docs/en/playbook_advanced_features.md) |
| **中文** | [文档中心](./docs/zh/README.md) · [API 文档](./docs/api.md) · [配置指南](./docs/zh/config_guide.md) · [Playbook 教程](./docs/zh/playbook_tutorial.md) · [高级特性](./docs/zh/playbook_advanced_features.md) |

## License

MIT — see [LICENSE](./LICENSE)
