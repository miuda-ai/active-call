# Active Call Documentation (English)

Welcome to the Active Call English documentation hub.

## 📚 Documentation Index

### Getting Started

- **[Configuration Guide](./config_guide.md)** — Complete guide for configuring Active Call with different calling scenarios

### Core Concepts

- **[API Documentation](../api.md)** — Full WebSocket API reference (commands, events, REST endpoints)
- **[Playbook Tutorial](./playbook_tutorial.md)** — Learn how to build stateful voice agents with Playbook
- **[Playbook Advanced Features](./playbook_advanced_features.md)** — Advanced Playbook capabilities
- **[Template Syntax Comparison](./template_syntax_comparison.md)** — Template syntax explanation and comparison

### Architecture & Integration

- **[A Decoupled Architecture for AI Voice Agent](./architecture.md)** — System architecture overview
- **[VoiceAgent Integration with Telephony Networks](./webrtc_sip.md)** — Technical implementation details

### Carrier Integration

- **[Twilio Elastic SIP Trunking](../twilio_integration.md)** — Connect active-call to Twilio (TLS + SRTP)
- **[Telnyx SIP Trunking](../telnyx_integration.md)** — Connect active-call to Telnyx

### Advanced Features

- **[Emotion Resonance](./features/emotion_resonance.md)** — Real-time emotional response system
- **[HTTP Tool Integration](./features/http_tool.md)** — External API integration via function calling
- **[Intent Clarification](./features/intent_clarification.md)** — Advanced intent detection and clarification
- **[Voice Emotion](./features/voice_emotion.md)** — Voice emotion analysis and synthesis
- **[Rolling Summary](./features/rolling_summary.md)** — Conversation context rolling summary
- **[Context Repair](./features/context_repair.md)** — Context repair and completion
- **[Tool Instructions](./features/tool_instructions.md)** — Tool usage instructions
- **[Context Management](./features/context_management.md)** — Conversation context management

## 🚀 Quick Start by Scenario

### Scenario 1: Browser-based Voice Chat (WebRTC)
→ See [Configuration Guide - WebRTC Scenario](./config_guide.md)
*(Note: Requires HTTPS or 127.0.0.1)*

### Scenario 2: Telephony Integration (SIP)
→ See [Configuration Guide - SIP Scenario](./config_guide.md)

### Scenario 3: Custom Application Integration (WebSocket)
→ See [Configuration Guide - WebSocket Scenario](./config_guide.md)

### Scenario 4: Inbound Call Handling (SIP Registration)
→ See [Configuration Guide - SIP Inbound](./config_guide.md)

## 📖 Additional Resources

- [Example Playbooks](../../config/playbook/) — Ready-to-use playbook examples
- [Example Code](../../examples/) — Sample implementations and utilities
- [中文文档](../zh/README.md) — Chinese Documentation
