# Playbook 高级特性完整指南

本文档介绍 Active-Call Playbook 系统中的高级特性，包括环境变量支持、SIP Headers 提取、变量管理、HTTP 调用等。

## 目录

- [环境变量支持 (Universal)](#环境变量支持-universal)
- [SIP Headers 提取与使用](#sip-headers-提取与使用)
- [变量管理 (`<set_var>`)](#变量管理-set_var)
- [HTTP 外部调用 (`<http>`)](#http-外部调用-http)
- [SIP BYE Headers 定制](#sip-bye-headers-定制)
- [完整流程示例](#完整流程示例)

---

## 环境变量支持 (Universal)

### ✨ 新特性：所有配置字段支持 `${VAR_NAME}` 语法

从 v0.3.37+ 开始，**所有 Playbook 配置字段**都支持环境变量模板语法。

### 语法

```yaml
# 字符串字段
provider: "${MY_PROVIDER}"
api_key: "${OPENAI_API_KEY}"

# 数值字段（不需要引号）
speed: ${TTS_SPEED}
temperature: ${LLM_TEMPERATURE}
max_tokens: ${LLM_MAX_TOKENS}

# 嵌套字段
base_url: "${OPENAI_BASE_URL}"
language: "${ASR_LANGUAGE}"
```

### 示例

```yaml
---
asr:
  provider: "${ASR_PROVIDER}"      # sensevoice, tencent, aliyun
  language: "${ASR_LANGUAGE}"      # zh, en, auto
  
tts:
  provider: "${TTS_PROVIDER}"      # supertonic, cosyvoice
  speaker: "${TTS_SPEAKER}"        # F1, M1, M2, F2
  speed: ${TTS_SPEED}              # 0.8, 1.0, 1.2
  
llm:
  provider: "${LLM_PROVIDER}"      # openai, azure, dashscope
  model: "${LLM_MODEL}"            # gpt-4o, gpt-4o-mini
  apiKey: "${LLM_API_KEY}"
  baseUrl: "${LLM_BASE_URL}"
  temperature: ${LLM_TEMPERATURE}  # 0.0 - 2.0
  max_tokens: ${LLM_MAX_TOKENS}    # integer
---
```

### 优势

1. **安全性**：API keys 不会被提交到代码仓库
2. **灵活性**：同一个 playbook，不同环境不同配置
3. **动态性**：可以在运行时切换模型、语言等参数
4. **通用性**：支持所有字段类型（字符串、数值、嵌套对象）

### 回退行为

- 如果环境变量未定义，`${VAR_NAME}` 会被保留（原样输出）
- YAML 解析器可能会因为无效值而失败
- 建议：始终设置必需的环境变量

### 完整示例

参见：[Environment Variables Example](../config/playbook/env_vars_example.md)

### ⚠️ 与运行时变量 `{{var}}` 的区别

**重要**：`${VAR}` 和 `{{var}}` 是两个**不同的能力**，不会冲突！

| 语法 | 用途 | 时机 | 作用域 | 来源 |
|------|------|------|--------|------|
| `${VAR}` | 环境变量 | Playbook 加载时（静态） | YAML 配置 | 系统环境变量 |
| `{{var}}` | 运行时变量 | 对话运行时（动态） | Prompt 文本 | SIP Headers、set_var |

**示例**：
```markdown
---
# 配置中使用 ${VAR} - 从环境变量读取
llm:
  apiKey: "${OPENAI_API_KEY}"  # ← 加载时替换
  model: "${LLM_MODEL}"
  
sip:
  extract_headers:
    - "X-Customer-Name"
---

# Prompt 中使用 {{var}} - 运行时替换
# Scene: main
你好，{{X-Customer-Name}}！     # ← 每次通话不同
```

**详细对比**：参见 [Template Syntax Comparison](template_syntax_comparison.md)

---

## SIP Headers 提取与使用

### 1. 配置提取规则

在 Playbook 的 YAML 配置部分指定要提取的 SIP Headers：

```yaml
---
sip:
  extract_headers:
    - "X-CID"           # 客户ID
    - "X-Session-Type"  # 会话类型
    - "X-Agent-ID"      # 坐席ID
llm:
  provider: "aliyun"
  model: "qwen-turbo"
---
```

### 2. 在 Playbook 中使用

提取的 Headers 会自动注入到 Playbook 的变量上下文中，可以使用 Jinja2 语法访问：

```markdown
你好！您的客户编号是 {{ X-CID }}。
本次会话类型：{{ X-Session-Type }}。
```

### 3. LLM 访问方式

LLM 可以通过系统消息获取这些变量（自动注入到上下文）：

```
用户: 我的编号是多少？
LLM: 根据系统记录，您的客户编号是 {{ X-CID }}。
```

---

## 变量管理 (`<set_var>`)

`<set_var>` 标签允许 LLM 在对话过程中动态设置或修改变量。

### 语法

```xml
<set_var key="变量名" value="变量值" />
```

### 使用场景

#### 1. 记录用户信息

```markdown
LLM: 请问您贵姓？
用户: 我姓张
LLM: 张先生您好 <set_var key="user_name" value="张先生" />，请问有什么可以帮您？
```

#### 2. 保存业务状态

```markdown
LLM: 已为您创建工单，工单号 12345 <set_var key="ticket_id" value="12345" />
```

#### 3. 设置 BYE Headers（用于 SIP）

```markdown
LLM: 通话结束，感谢您的来电 <set_var key="_sip_headers" value='{"X-Hangup-Reason":"normal","X-Duration":"180"}' />
```

### 特性说明

- **实时更新**: 变量立即写入 `ActiveCall.extras`
- **持久化**: 变量在整个通话会话中保持
- **可传递**: 可用于后续的 BYE 请求 Headers
- **类型**: 所有值都存储为字符串（JSON Value::String）

### 注意事项

⚠️ **变量值中的特殊字符需要转义**：

```xml
<!-- 正确 -->
<set_var key="note" value='{"status":"ok"}' />

<!-- 错误 - 双引号冲突 -->
<set_var key="note" value="{"status":"ok"}" />
```

---

## HTTP 外部调用 (`<http>`)

`<http>` 标签允许 LLM 在对话中调用外部 HTTP API。

### 语法

```xml
<http url="API地址" method="方法" body="请求体" />
```

- **url**: 必需，API 完整 URL
- **method**: 可选，默认 GET（支持 GET/POST/PUT）
- **body**: 可选，请求体内容

### 使用场景

#### 1. 查询外部数据

```markdown
LLM: 我来帮您查询天气 <http url="https://api.weather.com/v1/current?city=beijing" />
```

**响应处理**：HTTP 响应会自动添加到对话历史中：

```
系统: HTTP GET https://api.weather.com/v1/current?city=beijing returned (200): {"temp":15,"condition":"sunny"}
LLM: 北京当前温度 15 度，天气晴朗。
```

#### 2. 提交数据

```markdown
LLM: 正在为您创建工单 <http url="https://api.crm.com/tickets" method="POST" body='{"subject":"咨询","customer":"123"}' />
```

#### 3. 多步骤交互

```markdown
用户: 帮我订一张去上海的机票
LLM: 好的，我先查询一下航班信息 <http url="https://api.flight.com/search?dest=shanghai" />

[系统返回航班列表]

LLM: 有 MU5180 和 CA1234 两个航班，您选择哪个？
用户: MU5180
LLM: 正在为您预订 <http url="https://api.flight.com/book" method="POST" body='{"flight":"MU5180","passenger":"user123"}' />
```

### 特性说明

- **同步执行**: HTTP 请求会阻塞流式输出，等待响应
- **自动注入**: 响应内容自动添加到对话历史
- **错误处理**: 失败时错误信息也会添加到历史
- **安全性**: 建议只调用可信的内部 API

### 注意事项

⚠️ **性能考虑**：
- HTTP 调用会增加响应延迟
- 建议设置合理的超时时间
- 避免在流式响应中频繁调用

---

## SIP BYE Headers 定制

在 SIP 通话结束时，可以附加自定义 Headers 到 BYE 请求。

### 1. 配置模板

在 Playbook 配置中定义 BYE Headers 模板：

```yaml
---
sip:
  hangup_headers:
    X-Hangup-Reason: "{{ hangup_reason }}"
    X-Call-Duration: "{{ call_duration }}"
    X-User-Rating: "{{ user_rating }}"
    X-Ticket-ID: "{{ ticket_id }}"
---
```

### 2. LLM 设置变量

在对话中通过 `<set_var>` 设置这些变量：

```markdown
LLM: 问题已解决，请对本次服务评分（1-5）
用户: 5分
LLM: 感谢您的好评 <set_var key="user_rating" value="5" /> <set_var key="hangup_reason" value="satisfied" />，再见！<hangup/>
```

### 3. 自动渲染

系统会在发送 BYE 时：
1. 读取 `ActiveCall.extras` 中的所有变量
2. 使用 Jinja2 渲染 `hangup_headers` 模板
3. 将渲染结果作为 SIP Headers 发送

### 直接设置 Headers

也可以直接设置 `_sip_headers` 变量：

```xml
<set_var key="_sip_headers" value='{"X-Custom":"value","X-Status":"completed"}' />
```

> 注意：直接设置会覆盖模板渲染结果

---

## 完整流程示例

### 场景：客服呼叫中心

```yaml
---
asr:
  provider: "aliyun"
llm:
  provider: "openai"
  model: "gpt-4o"
  apiKey: "${OPENAI_API_KEY}"
  prompt: |
    你是一个客服机器人。可以使用以下工具：
    - 查询工单: <http url="https://api.crm.com/tickets/{ticket_id}" />
    - 创建工单: <http url="https://api.crm.com/tickets" method="POST" body='...' />
    - 记录信息: <set_var key="变量名" value="值" />
    
    当用户要求转人工时，输出: <refer to="sip:agent@domain.com"/>
    对话结束时，务必记录通话原因。
tts:
  provider: "aliyun"
sip:
  extract_headers:
    - "X-Customer-ID"
    - "X-Call-Source"
    - "X-Priority"
  hangup_headers:
    X-Hangup-Reason: "{{ hangup_reason }}"
    X-Resolved: "{{ is_resolved }}"
    X-Ticket-ID: "{{ ticket_id }}"
    X-Sentiment: "{{ user_sentiment }}"
---

你是客服助手小智。请注意：

1. 首先使用客户ID查询历史工单: <http url="https://api.crm.com/customers/{{ X-Customer-ID }}/tickets" />
2. 处理问题时创建工单并记录ID
3. 对话中识别用户情绪（positive/neutral/negative）
4. 结束时记录必要信息

礼貌、专业、简洁回复。
```

### 实际对话流程

```
[SIP INVITE with X-Customer-ID: CUST001]

机器人: 您好，我是小智客服。<http url="https://api.crm.com/customers/CUST001/tickets" />

[API返回: 该客户有1个未处理工单 #TK123]

机器人: 看到您有一个进行中的工单 #TK123，是关于这个问题吗？

客户: 不是，我有新问题，网络连不上

机器人: 好的，我帮您创建新工单 <http url="https://api.crm.com/tickets" method="POST" body='{"customer":"CUST001","subject":"网络连接故障","priority":"high"}' /> 
      <set_var key="user_sentiment" value="neutral" />

[API返回: 创建成功，工单号 TK456]

机器人: 工单 TK456 已创建 <set_var key="ticket_id" value="TK456" />。现在帮您排查...

[技术支持对话...]

机器人: 问题解决了吗？

客户: 好了，谢谢！

机器人: 太好了 <set_var key="is_resolved" value="true" /> <set_var key="user_sentiment" value="positive" /> <set_var key="hangup_reason" value="problem_solved" />！
        祝您使用愉快，再见！<hangup/>

[SIP BYE with Headers:]
X-Hangup-Reason: problem_solved
X-Resolved: true
X-Ticket-ID: TK456
X-Sentiment: positive
```

---

## 完整测试用例

参见 [tests.rs](../src/playbook/handler/tests.rs) 中的：

- `test_set_var_in_streaming`
- `test_set_var_with_sip_headers`
- `test_http_command_in_stream`
- `test_http_command_post_with_body`
- `test_multiple_commands_in_sequence`

---

## 最佳实践

### 1. 性能优化

✅ **推荐**：
```markdown
查询中 <http url="..." /> 处理中...
```

❌ **避免**：
```markdown
<http url="...1" /> <http url="...2" /> <http url="...3" />
```

### 2. 错误处理

在 LLM Prompt 中说明错误处理：

```
如果 API 调用失败，礼貌告知用户并提供备选方案。
不要暴露技术错误细节。
```

### 3. 安全建议

- ✅ 只调用内部可信 API
- ✅ 使用环境变量存储敏感信息（如 API Key）
- ✅ 在 API 网关层做访问控制
- ❌ 不要在 Playbook 中硬编码密钥
- ❌ 不要调用不可控的外部 URL

### 4. 变量命名规范

- 使用下划线分隔：`user_name`, `ticket_id`
- 保留前缀 `_sip_` 用于系统变量
- 避免使用特殊字符和空格

---

## 故障排查

### 问题：变量未生效

**检查**：
1. `<set_var>` 语法是否正确（注意引号）
2. 变量名是否在模板中拼写一致
3. 查看日志确认变量已写入 extras

### 问题：HTTP 调用无响应

**检查**：
1. URL 是否可访问（网络/防火墙）
2. API 是否有超时限制
3. 查看对话历史中的系统消息确认响应内容

### 问题：BYE Headers 未携带

**检查**：
1. `sip.hangup_headers` 配置是否正确
2. 变量在 hangup 前是否已设置
3. 确认是 SIP 类型通话（WebRTC 不支持）

---

## API 参考

### ActiveCall.extras

类型：`Option<HashMap<String, serde_json::Value>>`

存储通话会话中的所有变量，包括：
- 提取的 SIP Headers
- `<set_var>` 设置的变量
- 系统注入的变量

### Playbook 配置

```yaml
sip:
  extract_headers: [String]        # 要提取的 Headers 列表
  hangup_headers: HashMap<String, String>  # BYE Headers 模板
```

---

## 相关文档

- [Playbook 基础](./playbook_guide.md)
- [SIP 集成](./sip_integration.md)
- [API Reference](./api_reference.md)
