# Playbook 配置与使用完全指南

Playbook 是 RustPBX 的核心配置文件，它采用 Markdown 格式，通过 Front Matter (YAML) 定义 AI 引擎参数，通过 Markdown 内容定义业务流程、场景和 AI 提示词（Prompt）。

## 1. 基本结构

Playbook 文件由三部分组成：
1.  **Front Matter (`---`)**: YAML 格式的全局配置，包括 ASR/TTS/LLM 引擎参数。
2.  **全局提示词 (Global Prompt)**: 定义 AI 的角色身份、行为准则和工具使用规则。
3.  **场景 (`# Scene: ...`)**: 定义不同的对话阶段及其特定的提示词、按键处理和流程跳转。

---

## 2. 全局配置 (Front Matter)

### 2.1 基础引擎配置
```yaml
asr:
  provider: "aliyun" # 或 "openai", "tencent", "deepgram", "sensevoice"
  language: "zh-CN"
  # extra 参数用于向特定引擎传递额外配置
  extra:
    silence_threshold: "0.05" # 仅用于 sensevoice: 静音阈值 (默认 0.01)，调高可减少噪音误触发
tts:
  provider: "aliyun" # 默认值: 中文(zh)默认 aliyun, 英文(en)默认 supertonic
  model: "cosyvoice-v2"
  speed: 1.0
  volume: 50
llm:
  provider: "aliyun"
  model: "gpt-4o"
  #apiKey: "OPENAI_API_KEY"
  #baseUrl: "https://api.openai.com/v1"
  language: "zh" # 默认: "zh"。用于加载语言特定的工具说明和功能特性
  features: ["http_tool", "voice_emotion"] # 启用增强功能
  # toolInstructions: "自定义工具使用说明..." # 可选: 覆盖默认的工具使用说明
```

### 2.2 交互行为配置
```yaml
greeting: "您好，我是您的 AI 助手，请问有什么可以帮您？"
denoise: true # 启用语音降噪
agc: {} # 启用 WebRTC AGC2 自动增益控制（默认 5 dB 余量，15 dB 初始增益），需要同时配置 `vad`
interruption:
  strategy: "both" # 打断策略: "none", "vad", "asr", "both"
  minSpeechMs: 500 # 用户说话超过 500ms 才触发打断
  fillerWordFilter: true # 自动过滤 "嗯"、"那个" 等语气词
followup:
  timeout: 10000 # 如果用户 10 秒没说话，AI 主动开启跟进
  max: 2 # 最多连续跟进 2 次
```

### 2.3 辅助功能配置
```yaml
ambiance:
  path: "./config/office.wav" # 通话背景音乐
  duckLevel: 0.1 # AI 说话时背景音自动降低到的音量系数 (0.1 = 10%)
  normalLevel: 0.5 # 默认背景音量
recorder:
  recorderFile: "recordings/call_{id}.wav" # 自动开启通话录音
```

---

## 3. 场景管理 (Scenes)

通过 `Scene: [ID]` 定义对话阶段，AI 会在切换场景时自动更新其 System Prompt。

```markdown
# Scene: start
## welcome
AI 角色：你现在是欢迎人员。

<dtmf digit="1" action="goto" scene="sale"/>
<dtmf digit="2" action="transfer" target="sip:operator@domain.com"/>

# Scene: sale
## product_info
AI 角色：你现在是销售顾问。请向用户介绍我们的理财产品。
```

---

## 4. 动作指令 (Commands)

Playbook 支持两种方式触发系统动作：

### 4.1 XML 简易指令 (推荐)
这类指令可以直接写在角色描述或直接被模型输出，支持流式处理：

-   **挂断**: `<hangup/>`
-   **转接**: `<refer to="sip:1001@127.0.0.1"/>`
-   **发送 SIP 元信息正文**: `<message body="customer_id=12345"/>`
-   **音效播放**: `<play file="config/media/ding.wav"/>`
-   **场景跳转**: `<goto scene="support"/>`

### 4.2 JSON 工具调用 (自定义推理)
用于复杂操作，如 HTTP 调用。结果会自动喂回给 AI 进行下一次推理。

```json
{
  "tools": [
    {
      "name": "http",
      "url": "https://api.example.com/query",
      "method": "POST",
      "body": { "id": "123" }
    }
  ]
}
```

---

## 5. DTMF 数字收集

在智能客服场景中，对于电话号码、验证码、身份证号等数字信息，语音识别容易出错。DTMF (按键音) 数字收集功能提供了更可靠的输入方式。

### 5.1 配置收集器模板

在 Front Matter 中定义可复用的收集器配置：

```yaml
dtmfCollectors:
  phone:
    description: "11位手机号"
    digits: 11  # 固定位数
    finishKey: "#"  # 完成键（可选）
    timeout: 20  # 总超时（秒）
    interDigitTimeout: 5  # 按键间隔超时（秒）
    validation:
      pattern: "^1[3-9]\\d{9}$"  # 正则表达式验证
      errorMessage: "请输入有效的11位手机号，以1开头"
    retryTimes: 3  # 验证失败最大重试次数
    interruptible: false  # 收集时是否允许语音打断
  
  code:
    description: "6位验证码"
    digits: 6
    timeout: 30
    interDigitTimeout: 5
    retryTimes: 2
    # 无 finishKey：达到最大位数自动完成
  
  idcard:
    description: "身份证号"
    minDigits: 15  # 最少位数
    maxDigits: 18  # 最多位数
    finishKey: "#"
    timeout: 30
    validation:
      pattern: "^\\d{15}(\\d{2}[0-9X])?$"
      errorMessage: "请输入15或18位身份证号"
```

**配置说明**：
- `digits`: 固定位数（`digits: 6` 等同于 `minDigits: 6, maxDigits: 6`）
- `finishKey`: 完成键（`#` 或 `*`）。如果不设置且配置了 `maxDigits`，达到最大位数时自动完成
- `timeout`: 从开始收集到超时的总时长（秒）
- `interDigitTimeout`: 两次按键之间的超时（秒），超时后尝试验证已收集的数字
- `validation`: 正则表达式验证规则和错误提示（可选）
- `retryTimes`: 验证失败后的最大重试次数（默认 3 次）
- `interruptible`: 是否允许用户在收集过程中通过语音打断（默认 false）

### 5.2 LLM 调用收集器

定义好收集器后，系统会自动将可用的收集器信息注入到 LLM 的 System Prompt 中。LLM 可以通过输出 XML 标签来启动收集：

```markdown
你是智能客服。当需要收集用户的手机号时，使用：
<collect type="phone" var="user_phone" prompt="请输入您的11位手机号，输入完成后按井号键" />
```

**参数说明**：
- `type`: 收集器类型（对应 `dtmfCollectors` 中定义的 key）
- `var`: 变量名，收集成功后会存储到通话状态的 `extras` 中
- `prompt`: 语音提示（可选），告诉用户如何输入

### 5.3 收集流程

1.  **LLM 输出收集命令**：`<collect type="phone" var="user_phone" prompt="请输入手机号" />`
2.  **系统播放提示**：播放 prompt 中的文字（TTS）
3.  **进入收集模式**：用户只能按键输入，语音输入会被忽略（除非 `interruptible: true`）
4.  **收集完成**：
    - 用户按下完成键（如 `#`），或
    - 达到最大位数（如果未配置完成键），或
    - 按键间隔超时
5.  **验证**：
    - 检查最少位数（`minDigits`）
    - 匹配正则表达式（`validation.pattern`）
6.  **结果处理**：
    - **验证成功**：存储到 `{{ var_name }}`，通知 LLM 继续对话
    - **验证失败**：播放错误提示（`errorMessage`），重新收集（最多 `retryTimes` 次）
    - **重试超限**：通知 LLM 收集失败，由 LLM 引导用户（如转人工或尝试其他方式）

### 5.4 使用收集的变量

收集成功后，LLM 可以在后续对话中使用 Minijinja 模板语法引用变量：

```markdown
感谢您提供手机号：{{ user_phone }}。我们将向该号码发送验证码。

<collect type="code" var="sms_code" prompt="请输入您收到的6位短信验证码" />
```

**调试提示**：
- 收集成功/失败时，系统会向 LLM 发送 System 消息，可以在日志中查看
- 如果收集器类型不存在，系统会向 LLM 返回可用类型列表
- 超时时，如果已收集部分数字，系统会尝试验证；如果没有收集到任何数字，会通知 LLM

### 5.5 完整示例

```yaml
---
asr:
  provider: "aliyun"
tts:
  provider: "aliyun"
llm:
  provider: "openai"
  model: "gpt-4o"
  language: "zh"

dtmfCollectors:
  phone:
    description: "11位手机号"
    digits: 11
    finishKey: "#"
    validation:
      pattern: "^1[3-9]\\d{9}$"
      errorMessage: "请输入有效的11位手机号"
    retryTimes: 3
  
  code:
    description: "6位验证码"
    digits: 6
    retryTimes: 2
---

# 身份验证客服

你是银行客服。需要先验证用户身份：

1. 首先收集手机号：<collect type="phone" var="user_phone" prompt="请输入您的11位手机号，输入完成后按井号键" />
2. 收集验证码：<collect type="code" var="sms_code" prompt="请输入您收到的6位短信验证码" />
3. 验证成功后，为用户办理业务

对于输入错误，请友好引导用户重新输入。如果多次失败，提示转人工服务。
```

---

## 6. 进阶功能

### 6.1 Realtime 模式配置
如果模型支持 Realtime API (如 OpenAI gpt-4o-realtime)，可跳过序列化 pipeline 以获得超低延迟：

```yaml
realtime:
  provider: "openai"
  model: "gpt-4o-realtime-preview"
  voice: "alloy"
  turn_detection:
    type: "server_vad"
    threshold: 0.5
```

### 6.2 RAG (检索增强生成)
在 llm 配置中启用功能后，AI 可以调用内置的知识库检索逻辑。

### 6.3 自定义工具使用说明
默认情况下，系统会根据 `language` 配置（如 "en" 为英文，"zh" 为中文）在提示词中包含工具使用说明。这些说明告诉 LLM 如何使用 `<hangup/>`、`<refer/>` 等命令。

**方法1: 使用语言特定的默认说明**
在 LLM 配置中设置 `language` 字段：
```yaml
llm:
  language: "zh" # 将使用 features/tool_instructions.zh.md
```

**方法2: 提供自定义说明**
完全覆盖默认的工具使用说明：
```yaml
llm:
  toolInstructions: |
    针对您特定用例的自定义说明。
    您可以在这里定义自己的工具使用格式。
```

**方法3: 修改特性文件**
直接编辑文件：
- `features/tool_instructions.zh.md` 用于中文
- `features/tool_instructions.en.md` 用于英文
- 添加您自己的语言：`features/tool_instructions.ja.md` 用于日语

这允许您：
- 将工具说明翻译成任何语言
- 添加特定领域的指导
- 自定义说明的格式和风格

### 6.4 Post-hook (通话结果上报)
通话挂断后，自动生成摘要并推送到业务系统：

```yaml
posthook:
  url: "https://your-crm.com/api/callback"
  summary: "detailed" # 摘要精细度: short, detailed, intent, json
  include_history: true
```

---

## 7. 最佳实践规则

1.  **短句原则**: 在提示词中要求 AI 使用短句，因为系统会按句子流式合成语音，句子越短响应越快。
2.  **打断保护**: 如果 AI 说话很关键，可以在 Front Matter 中设置 `interruption.strategy: "none"` 临时禁止打断。
3.  **转接兜底**: 在提供转接功能时，务必告知 AI 如果转接失败该如何安抚用户。
6.  **变量注入**: Playbook 支持 Minijinja 模板语法，你可以在启动呼叫时动态注入变量。
    - 普通变量：`{{ user_name }}`
    - SIP Headers（包含连字符）：`{{ sip["X-Customer-ID"] }}`（详见[高级特性文档](playbook_advanced_features.md)）
