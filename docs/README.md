# Active Call Documentation

Welcome to the Active Call documentation hub. This directory contains comprehensive guides for building AI Voice Agents with Active Call.

## 📚 Documentation Index

### Getting Started

- **[Configuration Guide (EN)](./config_guide.en.md)** - Complete guide for configuring Active Call with different calling scenarios
- **[配置指南 (中文)](./config_guide.zh.md)** - Active Call 完整配置指南，包含不同呼叫场景的配置方案

### Core Concepts

- **[API Documentation](./api.md)** - Detailed WebSocket API reference for all call endpoints
- **[Playbook Tutorial (EN)](./playbook_tutorial.en.md)** - Learn how to build stateful voice agents with Playbook
- **[Playbook 教程 (中文)](./playbook_tutorial.zh.md)** - Playbook 编写指南与最佳实践

### Architecture & Integration

- **[How SIP Works](./sip.png)** - SIP call flow diagram
  - AppServer initiates calls via Active Call
  
- **[How WebRTC Works](./webrtc.png)** - WebRTC connection flow diagram
  - App establishes WebRTC connection via Active Call

- **[VoiceAgent Integration with Telephony Networks (EN)](how%20webrtc%20work%20with%20sip(en).md)** - Technical implementation details
- **[VoiceAgent 与电话网络互通的技术实现 (中文)](how%20webrtc%20work%20with%20sip(zh).md)** - 技术实现细节

- **[A Decoupled Architecture for AI Voice Agent Development (EN)](./a%20decoupled%20architecture%20for%20AI%20Voice%20Agent%20en.md)** - System architecture overview
- **[分体式VoiceAgent架构 (中文)](./a%20decoupled%20architecture%20for%20AI%20Voice%20Agent%20zh.md)** - 系统架构说明

### Advanced Features

Explore advanced features in the [`features/`](../features/) directory:
- **[Emotion Resonance](../features/emotion_resonance.en.md)** - Real-time emotional response system
- **[HTTP Tool Integration](../features/http_tool.en.md)** - External API integration via function calling
- **[Intent Clarification](../features/intent_clarification.en.md)** - Advanced intent detection and clarification
- **[Voice Emotion](../features/voice_emotion.en.md)** - Voice emotion analysis and synthesis

## 🚀 Quick Start by Scenario

### Scenario 1: Browser-based Voice Chat (WebRTC)
→ See [Configuration Guide - WebRTC Scenario](./config_guide.en.md#scenario-1-webrtc-calls-browser-communication)

### Scenario 2: Telephony Integration (SIP)
→ See [Configuration Guide - SIP Scenario](./config_guide.en.md#scenario-2-sip-calls-pbxfs-gateway-integration)

### Scenario 3: Custom Application Integration (WebSocket)
→ See [Configuration Guide - WebSocket Scenario](./config_guide.en.md#scenario-3-websocket-calls-custom-applications)

### Scenario 4: Inbound Call Handling (SIP Registration)
→ See [Configuration Guide - SIP Inbound](./config_guide.en.md#scenario-4-sip-inbound-register-to-pbx)

## 📖 Additional Resources

- [Example Playbooks](../config/playbook/) - Ready-to-use playbook examples
- [Example Code](../examples/) - Sample implementations and utilities
- [Bank Demo](../bank-demo/) - Complete demo application

---

For issues, questions, or contributions, please visit the project repository.
