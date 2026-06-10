# Playbook Configuration & Usage Guide

Playbook is the core configuration system of RustPBX. It uses the Markdown format, where the Front Matter (YAML) defines AI engine parameters, and the Markdown content defines the business flow, scenes, and AI prompts.

## 1. Basic Structure

A Playbook file consists of three parts:
1.  **Front Matter (`---`)**: YAML format for global configurations, including ASR, TTS, and LLM engine parameters.
2.  **Global Prompt**: Defines the AI's persona, behavior guidelines, and tool usage rules.
3.  **Scenes (`# Scene: ...`)**: Defines different stages of the conversation with their own specific prompts, DTMF handling, and workflow transitions.

---

## 2. Global Configuration (Front Matter)

### 2.1 Engine Configuration
```yaml
asr:
  provider: "openai" # Options: "openai", "aliyun", "tencent", "deepgram", "sensevoice"
  language: "en-US"
  # extra parameter for passing specific engine configurations
  extra:
    silence_threshold: "0.05" # Only for sensevoice: silence threshold (default 0.01), increase to reduce noise triggers
tts:
  provider: "supertonic" # Default: "supertonic" for English (en), "aliyun" for Chinese (zh)
  model: "M1"
  speed: 1.0
  volume: 50
llm:
  provider: "openai"
  model: "gpt-4o"
  apiKey: "OPENAI_API_KEY"
  #baseUrl: "https://api.openai.com/v1"
  language: "en" # Default: "zh". Used for loading language-specific tool instructions and features
  features: ["http_tool", "voice_emotion"] # Enable enhanced capabilities
  # toolInstructions: "Custom tool instructions..." # Optional: Override default tool usage instructions
```

### 2.2 Interaction Behavior
```yaml
greeting: "Hello, I am your AI assistant. How can I help you today?"
denoise: true # Enable noise reduction
interruption:
  strategy: "both" # Strategies: "none", "vad", "asr", "both"
  minSpeechMs: 500 # User must speak for at least 500ms to trigger interruption
  fillerWordFilter: true # Automatically filter fillers like "um", "ah", "uh"
followup:
  timeout: 10000 # AI proactively speaks if user is silent for 10 seconds
  max: 2 # Maximum number of consecutive follow-ups
```

### 2.3 Add-on Features
```yaml
ambiance:
  path: "./config/office.wav" # Background music for the call
  duckLevel: 0.1 # Volume reduction factor for background music when AI speaks (0.1 = 10%)
  normalLevel: 0.5 # Default background volume
recorder:
  recorderFile: "recordings/call_{id}.wav" # Automatically record the call
```

---

## 3. Scene Management

Define conversation stages via `# Scene: [ID]`. The AI will automatically update its System Prompt when switching scenes.

```markdown
# Scene: start
## welcome
AI Role: You are a receptionist welcoming the user.

<dtmf digit="1" action="goto" scene="sales"/>
<dtmf digit="2" action="transfer" target="sip:support@domain.com"/>

# Scene: sales
## product_info
AI Role: You are a sales consultant. Introduce our products to the user.
```

---

## 4. Action Commands

The Playbook supports two ways to trigger system actions:

### 4.1 XML Commands (Recommended)
Simple tags that can be output by the model during streaming for real-time execution:

-   **Hang up**: `<hangup/>`
-   **Transfer**: `<refer to="sip:1001@127.0.0.1"/>`
-   **Send SIP metadata body**: `<message body="customer_id=12345"/>`
-   **Play audio file**: `<play file="config/media/ding.wav"/>`
-   **Switch scene**: `<goto scene="support"/>`

### 4.2 JSON Tool Calling
Used for complex operations like HTTP calls. Results are fed back to the AI for a follow-up response.

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

## 5. DTMF Digit Collection

In customer service scenarios, collecting numeric information like phone numbers, verification codes, and ID numbers through voice recognition can be error-prone. DTMF (Dual-Tone Multi-Frequency) digit collection provides a more reliable input method.

### 5.1 Configure Collector Templates

Define reusable collector configurations in the Front Matter:

```yaml
dtmfCollectors:
  phone:
    description: "11-digit phone number"
    digits: 11  # Fixed digit count
    finishKey: "#"  # Finish key (optional)
    timeout: 20  # Total timeout (seconds)
    interDigitTimeout: 5  # Inter-digit timeout (seconds)
    validation:
      pattern: "^1[3-9]\\d{9}$"  # Regex validation
      errorMessage: "Please enter a valid 11-digit phone number starting with 1"
    retryTimes: 3  # Maximum retries on validation failure
    interruptible: false  # Allow voice interruption during collection
  
  code:
    description: "6-digit verification code"
    digits: 6
    timeout: 30
    interDigitTimeout: 5
    retryTimes: 2
    # No finishKey: auto-completes when max digits reached
  
  idcard:
    description: "ID card number"
    minDigits: 15  # Minimum digits
    maxDigits: 18  # Maximum digits
    finishKey: "#"
    timeout: 30
    validation:
      pattern: "^\\d{15}(\\d{2}[0-9X])?$"
      errorMessage: "Please enter a 15 or 18-digit ID number"
```

**Configuration Details**:
- `digits`: Fixed digit count (`digits: 6` is equivalent to `minDigits: 6, maxDigits: 6`)
- `finishKey`: Completion key (`#` or `*`). If not set and `maxDigits` is configured, auto-completes when reaching max digits
- `timeout`: Total duration from start to timeout (seconds)
- `interDigitTimeout`: Timeout between consecutive key presses (seconds), attempts validation on timeout
- `validation`: Regex validation rule and error message (optional)
- `retryTimes`: Maximum retry attempts after validation failure (default: 3)
- `interruptible`: Whether user can interrupt via voice during collection (default: false)

### 5.2 LLM Invokes Collectors

Once collectors are defined, the system automatically injects available collector information into the LLM's System Prompt. The LLM can start collection by outputting an XML tag:

```markdown
You are an AI customer service agent. When you need to collect the user's phone number, use:
<collect type="phone" var="user_phone" prompt="Please enter your 11-digit phone number, then press the pound key" />
```

**Parameters**:
- `type`: Collector type (corresponds to a key in `dtmfCollectors`)
- `var`: Variable name, stored in call state `extras` upon successful collection
- `prompt`: Voice prompt (optional), tells the user how to input

### 5.3 Collection Flow

1.  **LLM outputs collection command**: `<collect type="phone" var="user_phone" prompt="Please enter your phone number" />`
2.  **System plays prompt**: Text-to-speech (TTS) plays the prompt
3.  **Enter collection mode**: User can only input via keypad; voice input is ignored (unless `interruptible: true`)
4.  **Collection completes**:
    - User presses finish key (e.g., `#`), or
    - Reaches maximum digits (if no finish key configured), or
    - Inter-digit timeout occurs
5.  **Validation**:
    - Checks minimum digits (`minDigits`)
    - Matches regex pattern (`validation.pattern`)
6.  **Result handling**:
    - **Validation succeeds**: Stores in `{{ var_name }}`, notifies LLM to continue conversation
    - **Validation fails**: Plays error message (`errorMessage`), retries collection (up to `retryTimes`)
    - **Max retries exceeded**: Notifies LLM of failure, LLM guides user (e.g., transfer to agent or try alternative method)

### 5.4 Using Collected Variables

After successful collection, the LLM can reference variables using Minijinja template syntax in subsequent conversations:

```markdown
Thank you for providing your phone number: {{ user_phone }}. We will send a verification code to this number.

<collect type="code" var="sms_code" prompt="Please enter the 6-digit verification code you received" />
```

**Debugging Tips**:
- On collection success/failure, the system sends a System message to the LLM (visible in logs)
- If collector type doesn't exist, system returns a list of available types to the LLM
- On timeout, if some digits were collected, system attempts validation; if no digits collected, notifies LLM

### 5.5 Complete Example

```yaml
---
asr:
  provider: "openai"
tts:
  provider: "supertonic"
llm:
  provider: "openai"
  model: "gpt-4o"
  language: "en"

dtmfCollectors:
  phone:
    description: "11-digit phone number"
    digits: 11
    finishKey: "#"
    validation:
      pattern: "^1[3-9]\\d{9}$"
      errorMessage: "Please enter a valid 11-digit phone number"
    retryTimes: 3
  
  code:
    description: "6-digit verification code"
    digits: 6
    retryTimes: 2
---

# Identity Verification Agent

You are a bank customer service agent. You must verify the user's identity first:

1. First collect phone number: <collect type="phone" var="user_phone" prompt="Please enter your 11-digit phone number, then press the pound key" />
2. Collect verification code: <collect type="code" var="sms_code" prompt="Please enter the 6-digit verification code you received" />
3. After successful verification, assist the user with their request

For input errors, guide the user kindly to re-enter. If multiple failures occur, offer to transfer to a human agent.
```

---

## 6. Advanced Features

### 6.1 Realtime Mode
If using a model that supports the Realtime API (e.g., OpenAI gpt-4o-realtime), you can enable ultra-low latency mode:

```yaml
realtime:
  provider: "openai"
  model: "gpt-4o-realtime-preview"
  voice: "alloy"
  turn_detection:
    type: "server_vad"
    threshold: 0.5
```

### 6.2 RAG (Retrieval-Augmented Generation)
When enabled in the LLM config, the AI can call built-in knowledge base retrieval logic.

### 6.3 Customizing Tool Instructions
By default, the system includes tool usage instructions in the prompt based on the `language` setting (e.g., English for "en", Chinese for "zh"). These instructions tell the LLM how to use commands like `<hangup/>`, `<refer/>`, etc.

**Method 1: Use Language-Specific Defaults**
Set the `language` field in the LLM config:
```yaml
llm:
  language: "en" # Will use features/tool_instructions.en.md
```

**Method 2: Provide Custom Instructions**
Override the default tool instructions entirely:
```yaml
llm:
  toolInstructions: |
    Custom instructions for your specific use case.
    You can define your own tool usage format here.
```

**Method 3: Modify Feature Files**
Edit the files directly:
- `features/tool_instructions.en.md` for English
- `features/tool_instructions.zh.md` for Chinese
- Add your own language: `features/tool_instructions.ja.md` for Japanese

This allows you to:
- Translate tool instructions to any language
- Add domain-specific guidance
- Customize the format and style of instructions

### 6.4 Post-hook (Reporting)
Automatically generate a summary and push it to your business system after the call ends:

```yaml
posthook:
  url: "https://your-crm.com/api/callback"
  summary: "detailed" # Types: short, detailed, intent, json
  include_history: true
```

---

## 7. Best Practices

1.  **Short Sentences**: Instruct the AI to use short sentences. The system synthesizes audio per sentence; shorter sentences lead to faster responses.
2.  **Interruption Protection**: If the AI's speech is critical, set `interruption.strategy: "none"` temporarily in the Front Matter.
3.  **Transfer Fallback**: When offering transfers, always instruct the AI on how to handle failed transfers politely.
4.  **Variable Injection**: Playbooks support Minijinja templates. You can inject dynamic variables مانند `{{ user_name }}` when starting a call.
