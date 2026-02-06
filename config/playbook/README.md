# Playbook 示例集合

本目录包含各种 Active-Call Playbook 配置示例。

## 📚 示例列表

### 基础示例

1. **[demo.md](./demo.md)** - 最简单的问候示例
   - 适合新手入门
   - 展示基本的 ASR/TTS/LLM 配置

2. **[hello.md](./hello.md)** - Hello World 示例
   - 最小化配置
   - 快速验证系统运行

3. **[multi_scene.md](./multi_scene.md)** - 多场景切换
   - 演示场景管理
   - DTMF 按键交互

### 进阶示例

4. **[simple_crm.md](./simple_crm.md)** ⭐ - 简单 CRM 客服
   - SIP Headers 提取
   - 变量记录
   - BYE Headers 定制
   - **推荐用于学习 Headers 流程**

5. **[webhook_example.md](./webhook_example.md)** ⭐ - HTTP API 集成
   - 外部 API 调用
   - 数据获取与提交
   - **推荐用于学习 HTTP 工具**

6. **[advanced_example.md](./advanced_example.md)** 🚀 - 完整智能客服系统
   - 综合所有高级特性
   - 真实业务场景
   - 包含完整工作流程
   - **推荐用于生产环境参考**

## 🎯 快速开始

### 1. 选择示例

根据需求选择合适的示例：

```bash
# 初学者
cp config/playbook/demo.md config/playbook/my_first.md

# 需要 SIP 集成
cp config/playbook/simple_crm.md config/playbook/my_sip.md

# 需要 HTTP 调用
cp config/playbook/webhook_example.md config/playbook/my_webhook.md

# 完整功能
cp config/playbook/advanced_example.md config/playbook/my_advanced.md
```

### 2. 配置环境变量

编辑 `.env` 或在配置中替换：

```bash
export OPENAI_API_KEY="sk-..."
export ALIYUN_API_KEY="sk-..."
```

### 3. 启动测试

```bash
# WebRTC 方式（浏览器测试）
cargo run -- --config active-call.toml

# SIP 方式（需要 SIP 客户端）
# 配置 SIP 注册后拨打对应号码
```

## 📖 特性对照表

| 特性 | demo | hello | multi_scene | simple_crm | webhook | advanced |
|-----|------|-------|-------------|------------|---------|----------|
| 基础对话 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 多场景 | ❌ | ❌ | ✅ | ❌ | ❌ | ✅ |
| DTMF | ❌ | ❌ | ✅ | ❌ | ❌ | ✅ |
| SIP Headers | ❌ | ❌ | ❌ | ✅ | ❌ | ✅ |
| `<set_var>` | ❌ | ❌ | ❌ | ✅ | ❌ | ✅ |
| `<http>` | ❌ | ❌ | ❌ | ❌ | ✅ | ✅ |
| BYE Headers | ❌ | ❌ | ❌ | ✅ | ❌ | ✅ |
| 转人工 | ❌ | ❌ | ✅ | ✅ | ❌ | ✅ |
| 完整业务流程 | ❌ | ❌ | ❌ | ⚠️ | ⚠️ | ✅ |

✅ 完整支持 | ⚠️ 简单演示 | ❌ 不包含

## 🔧 配置说明

### YAML 配置部分

```yaml
---
# ASR 配置
asr:
  provider: "aliyun"  # 或 "azure", "openai"
  
# LLM 配置
llm:
  provider: "openai"  # 或 "aliyun", "azure"
  model: "gpt-4o"
  apiKey: "${API_KEY}"
  
# TTS 配置
tts:
  provider: "aliyun"
  
# SIP 配置（仅 SIP 呼叫需要）
sip:
  extract_headers: ["X-Header-Name"]
  hangup_headers:
    X-Custom: "{{ variable }}"
---
```

### Markdown Prompt 部分

紧跟在 YAML 后的 Markdown 内容是 LLM 的系统提示词。

## 🎨 自定义 Playbook

### 步骤 1: 创建文件

```bash
touch config/playbook/my_bot.md
```

### 步骤 2: 编写配置

```yaml
---
asr:
  provider: "aliyun"
llm:
  provider: "openai"
  model: "gpt-4o"
  apiKey: "${OPENAI_API_KEY}"
  prompt: |
    你是 [角色描述]
tts:
  provider: "aliyun"
---

[这里写详细的 System Prompt]
```

### 步骤 3: 测试

```bash
# 通过 API 指定 playbook
curl -X POST http://localhost:3000/call \
  -H "Content-Type: application/json" \
  -d '{"playbook": "my_bot.md"}'
```

## 📚 深入学习

- **[Playbook 高级特性文档](../docs/playbook_advanced_features.md)** - 详细特性说明
- **[测试用例](../src/playbook/handler/tests.rs)** - 查看单元测试了解实现细节
- **[API 文档](../docs/api.md)** - 完整 API 参考

## 💡 最佳实践

### 1. Prompt 设计

✅ **清晰的角色定义**
```
你是专业的客服助手，负责...
```

✅ **明确的工具说明**
```
可用工具：
- <set_var key="..." value="..." />
- <http url="..." />
```

✅ **提供示例对话**
```
示例：
用户: ...
你: ...
```

### 2. 变量命名

✅ 使用描述性名称：`user_name`, `ticket_id`  
❌ 避免：`var1`, `x`, `temp`

### 3. 错误处理

在 Prompt 中说明错误情况：
```
如果 API 调用失败，礼貌告知用户...
```

### 4. 性能优化

- HTTP 调用会增加延迟，合理使用
- 避免过长的 Prompt（影响响应速度）
- 使用流式输出提升体验

## 🐛 常见问题

### Q: Playbook 不生效？

A: 检查：
1. 文件路径是否正确（`config/playbook/xxx.md`）
2. YAML 格式是否正确（注意缩进）
3. 日志中是否有错误信息

### Q: 变量未传递到 BYE Headers？

A: 确保：
1. 在 SIP 通话中（WebRTC 不支持）
2. `sip.hangup_headers` 已配置
3. 变量在挂断前已设置

### Q: HTTP 调用失败？

A: 检查：
1. URL 是否可访问
2. 网络/防火墙配置
3. API 是否需要认证

## 🤝 贡献示例

欢迎贡献更多示例！

1. Fork 项目
2. 在 `config/playbook/` 创建新示例
3. 更新本 README
4. 提交 PR

示例命名规范：`[用途]_[特性].md`

例如：
- `customer_service_basic.md`
- `order_assistant_webhook.md`
- `survey_bot_multilang.md`

## 📝 更新日志

- **2024-02**: 添加高级特性示例（SIP Headers, set_var, http）
- **2024-01**: 添加多场景示例
- **2023-12**: 初始版本

---

有问题？查看[完整文档](../docs/playbook_advanced_features.md)或提交 [Issue](https://github.com/your-repo/issues)。
