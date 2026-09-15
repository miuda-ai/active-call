# Active Call 文档（中文）

欢迎来到 Active Call 中文文档中心。

## 📚 文档目录

### 快速入门

- **[配置指南](./config_guide.md)** — Active Call 完整配置指南，包含不同呼叫场景的配置方案

### 核心概念

- **[API 文档](../api.md)** — WebSocket API 完整参考（命令、事件、REST 端点）
- **[Playbook 教程](./playbook_tutorial.md)** — 如何使用 Playbook 构建有状态的语音 Agent
- **[Playbook 高级特性](./playbook_advanced_features.md)** — 高级 Playbook 功能详解
- **[模板语法对照](./template_syntax_comparison.md)** — 模板语法说明与对比

### 架构与集成

- **[分体式 VoiceAgent 架构](./architecture.md)** — 系统架构设计说明
- **[WebRTC 与 SIP 互通](./webrtc_sip.md)** — VoiceAgent 与电话网络互通技术实现

### 高级特性

- **[情感共鸣](./features/emotion_resonance.md)** — 实时情感响应系统
- **[HTTP 工具集成](./features/http_tool.md)** — 通过 Function Calling 集成外部 API
- **[意图澄清](./features/intent_clarification.md)** — 高级意图检测与澄清
- **[声音情感](./features/voice_emotion.md)** — 语音情感分析与合成
- **[滚动摘要](./features/rolling_summary.md)** — 对话上下文滚动摘要
- **[上下文修复](./features/context_repair.md)** — 上下文修复与补全
- **[工具使用说明](./features/tool_instructions.md)** — 工具调用使用规范
- **[上下文管理](./features/context_management.md)** — 对话上下文管理

## 🚀 场景快速入口

### 场景一：浏览器语音对话（WebRTC）
→ 参见 [配置指南 - WebRTC 场景](./config_guide.md)
*(注意：需要 HTTPS 或 127.0.0.1)*

### 场景二：电话集成（SIP）
→ 参见 [配置指南 - SIP 场景](./config_guide.md)

### 场景三：自定义应用集成（WebSocket）
→ 参见 [配置指南 - WebSocket 场景](./config_guide.md)

### 场景四：接入来电（SIP 注册）
→ 参见 [配置指南 - SIP 接入](./config_guide.md)

## 📖 其他资源

- [示例 Playbook](../../config/playbook/) — 可直接使用的 Playbook 示例
- [示例代码](../../examples/) — 示例实现与工具
- [英文文档](../en/README.md) — English Documentation
