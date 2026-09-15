# Template Syntax Comparison: `{{var}}` vs `${VAR}`

## 概述

Active-Call 支持两种模板语法，它们**不冲突**，在不同阶段工作，服务于不同目的。

## 对比表格

| 特性 | `${VAR}` | `{{var}}` |
|------|----------|-----------|
| **用途** | 环境变量替换 | 运行时变量替换 |
| **执行时机** | Playbook 加载时（静态） | 对话运行时（动态） |
| **作用域** | YAML 配置部分 | Prompt 文本部分 |
| **变量来源** | 系统环境变量 | SIP Headers、set_var、状态 |
| **处理引擎** | Rust regex | MiniJinja 模板引擎 |
| **典型用途** | API Keys、配置参数 | 客户信息、对话状态 |

## 执行顺序

```
┌─────────────────────────────────────────────────────────┐
│ 1. Playbook 文件加载                                      │
│    *.md file on disk                                     │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 2. Jinja2 渲染 (如果传入 variables 参数)                  │
│    {{var}} → 运行时变量替换                               │
│    用于动态生成 playbook 内容（可选）                      │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 3. 分离 YAML 和 Prompt                                    │
│    Split by "---"                                        │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 4. 环境变量替换 (在 YAML 部分)                            │
│    ${VAR} → 环境变量值                                    │
│    ⚠️ 只在 YAML 配置中生效                                │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 5. YAML 解析                                              │
│    serde_yaml::from_str()                                │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 6. Prompt 部分保留原样                                     │
│    {{var}} 保留在 prompt 中，等待运行时替换               │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 7. 通话开始 - 运行时变量注入                               │
│    - 提取 SIP Headers → {{X-CID}}                        │
│    - LLM set_var → {{user_name}}                         │
│    - Prompt 渲染时替换 {{var}}                            │
└─────────────────────────────────────────────────────────┘
```

## 详细说明

### `${VAR}` - 环境变量（配置时）

**时机**：Playbook 加载时，YAML 解析**之前**  
**位置**：只在 YAML 配置部分（`---` 之间）  
**来源**：系统环境变量

```markdown
---
llm:
  apiKey: "${OPENAI_API_KEY}"    # ← 这里使用 ${VAR}
  baseUrl: "${OPENAI_BASE_URL}"
  model: "${OPENAI_MODEL}"
tts:
  speed: ${TTS_SPEED}             # 数值也可以
---
# Scene: main
Your prompt here
```

**处理流程**：
1. 读取环境变量 `OPENAI_API_KEY`
2. 替换 `"${OPENAI_API_KEY}"` → `"sk-actual-key"`
3. 然后 YAML 解析器看到的是：`apiKey: "sk-actual-key"`

### `{{var}}` - 运行时变量（对话时）

**时机**：对话运行时，每次生成 prompt  
**位置**：主要在 Prompt 文本部分（`---` 之后）  
**来源**：SIP Headers、LLM set_var 命令、状态变量

```markdown
---
sip:
  extract_headers:
    - "X-CID"
    - "X-Name"
llm:
  provider: "openai"
---
# Scene: main

你好，{{X-Name}} 先生/女士！
您的客户编号是：{{X-CID}}
我们的对话轮次：{{turn_count}}
```

**处理流程**：
1. 通话开始，提取 SIP Header: `X-Name=张三`
2. 生成 prompt 时，替换 `{{X-Name}}` → `张三`
3. 每次对话都可能有不同的值

## 实际案例

### 案例 1：两者配合使用

```markdown
---
# 配置部分：使用 ${VAR} 从环境变量读取
asr:
  provider: "${ASR_PROVIDER}"      # 环境变量：sensevoice
  language: "${ASR_LANGUAGE}"      # 环境变量：zh
  
tts:
  provider: "${TTS_PROVIDER}"      # 环境变量：supertonic
  speaker: "${TTS_SPEAKER}"        # 环境变量：F1
  
llm:
  provider: "${LLM_PROVIDER}"      # 环境变量：openai
  apiKey: "${OPENAI_API_KEY}"      # 环境变量：sk-xxx
  model: "${LLM_MODEL}"            # 环境变量：gpt-4o
  
sip:
  extract_headers:
    - "X-Customer-ID"
    - "X-Customer-Name"
    - "X-VIP-Level"
---

# Prompt 部分：使用 {{var}} 运行时替换
# Scene: greeting

你好，{{X-Customer-Name}}！
您的客户编号是：{{X-Customer-ID}}
您是我们的 {{X-VIP-Level}} 客户。

我是 AI 助手，今天有什么可以帮您的？
```

**执行结果**：

1. **加载时（${VAR} 替换）**：
   ```yaml
   asr:
     provider: "sensevoice"
     language: "zh"
   tts:
     provider: "supertonic"
     speaker: "F1"
   llm:
     provider: "openai"
     apiKey: "sk-actual-key-here"
     model: "gpt-4o"
   ```

2. **运行时（{{var}} 替换）**：
   ```
   你好，张三！
   您的客户编号是：C12345
   您是我们的 Gold 客户。
   ```

### 案例 2：在 YAML 中使用 {{var}} - 不推荐

```markdown
---
llm:
  apiKey: "{{api_key}}"  # ❌ 不推荐！这会失败
---
```

**为什么失败？**
- `{{api_key}}` 在 YAML 解析时就存在
- YAML 解析器会把它当作字面字符串 `"{{api_key}}"`
- LLM 配置拿到的是无效的 API key

**正确做法**：
```markdown
---
llm:
  apiKey: "${OPENAI_API_KEY}"  # ✅ 使用环境变量
---
```

### 案例 3：在 Prompt 中使用 ${VAR} - 会保留

```markdown
---
llm:
  provider: "openai"
---
# Scene: main

Hello, I'm using model: ${LLM_MODEL}
```

**结果**：
- 如果 `LLM_MODEL` 环境变量存在：`"Hello, I'm using model: gpt-4o"`
- 如果不存在：`"Hello, I'm using model: ${LLM_MODEL}"` (保留原样)

## 变量来源对比

### `${VAR}` 来源

```bash
# 从环境变量
export OPENAI_API_KEY=sk-xxx
export TTS_SPEED=1.2
export ASR_LANGUAGE=zh

# Docker 中
docker run -e OPENAI_API_KEY=sk-xxx ...

# 或者 .env 文件
OPENAI_API_KEY=sk-xxx
TTS_SPEED=1.2
```

### `{{var}}` 来源

```markdown
# 1. SIP Headers
sip:
  extract_headers:
    - "X-CID"         # → {{X-CID}}

# 2. LLM set_var 命令
<set_var key="user_name" value="张三" />
# → {{user_name}}

# 3. HTTP 响应
<http url="..." method="GET" set_var="api_response" />
# → {{api_response}}

# 4. 系统内置
{{turn_count}}      # 对话轮次
{{summary}}         # 对话摘要
{{session_id}}      # 会话唯一标识
{{call_type}}       # 通话类型 (sip/websocket/webrtc/b2bua)
{{caller}}          # 主叫方 (仅 SIP)
{{callee}}          # 被叫方 (仅 SIP)
{{start_time}}      # 通话开始时间 (RFC 3339)
```

## 常见问题

### Q1: 能在 YAML 中使用 {{var}} 吗？

**A**: 技术上可以，但不推荐。

```markdown
---
# 在 parse() 之前传入 variables 参数
llm:
  model: "{{model_name}}"  # 可以工作，但容易混淆
---
```

这会在 **Jinja2 渲染阶段**（第2步）被替换，早于环境变量替换。但这会让配置变得复杂，建议用 `${VAR}` 代替。

### Q2: 能在 Prompt 中使用 ${VAR} 吗？

**A**: 可以，但会在 Playbook 加载时就被替换成固定值。

```markdown
---
llm:
  provider: "openai"
---
# Scene: main

Hello! API Endpoint: ${OPENAI_BASE_URL}
```

这个 `${OPENAI_BASE_URL}` 会在加载时被替换，所有通话都看到同样的值。  
如果你想要动态值，应该用 `{{var}}`。

### Q3: 两者能嵌套吗？

**A**: 不能直接嵌套，但可以组合使用。

```markdown
# ❌ 不支持
apiKey: "${OPENAI_${ENV}_KEY}"  # 不会展开

# ✅ 正确做法
export OPENAI_DEV_KEY=sk-dev
export API_KEY_VAR=OPENAI_DEV_KEY

# 然后在 shell 中：
export FINAL_KEY=$(eval echo \$$API_KEY_VAR)
```

### Q4: 优先级是什么？

执行顺序决定优先级：

1. **Jinja2** `{{var}}` - 如果在 parse() 时传入 variables
2. **环境变量** `${VAR}` - 在 YAML 解析前
3. **运行时** `{{var}}` - 在对话过程中

## 最佳实践

### ✅ 推荐做法

```markdown
---
# 配置：静态值用 ${VAR}
asr:
  provider: "${ASR_PROVIDER}"
  language: "${ASR_LANGUAGE}"
  
tts:
  provider: "${TTS_PROVIDER}"
  speaker: "${TTS_SPEAKER}"
  
llm:
  apiKey: "${OPENAI_API_KEY}"
  model: "${LLM_MODEL}"
  
sip:
  extract_headers:
    - "X-Customer-ID"
    - "X-Customer-Name"
---

# Prompt：动态值用 {{var}}
# Scene: main

你好，{{X-Customer-Name}}！
客户编号：{{X-Customer-ID}}

<set_var key="greeting_done" value="true" />
```

### ❌ 避免做法

```markdown
---
# ❌ 在 YAML 中用运行时变量
llm:
  apiKey: "{{runtime_api_key}}"  # 会失败

# ❌ 混淆两种语法
tts:
  speaker: "${TTS_SPEAKER}}"  # 多了一个 }
---

# ❌ 在 Prompt 中期望动态配置
# Scene: main
Current model: ${LLM_MODEL}  # 这是静态的，加载时就固定了
```

## 总结

| 场景 | 使用 | 原因 |
|------|------|------|
| API Keys | `${OPENAI_API_KEY}` | 敏感信息，从环境变量读取 |
| 模型配置 | `${LLM_MODEL}` | 配置参数，部署时确定 |
| TTS 参数 | `${TTS_SPEED}` | 静态配置，加载时确定 |
| 客户信息 | `{{X-Customer-Name}}` | 每次通话不同 |
| 对话状态 | `{{user_confirmed}}` | 运行时动态变化 |
| HTTP 响应 | `{{api_response}}` | 运行时获取 |
| 会话信息 | `{{session_id}}` | 内置自动注入 |
| 通话类型 | `{{call_type}}` | 内置自动注入 |

**记住**：
- **`${VAR}`** = 配置时（静态）= 环境变量
- **`{{var}}`** = 运行时（动态）= 对话状态

两者互补，不冲突！🎯
