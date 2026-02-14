# Template Syntax Comparison: `{{var}}` vs `${VAR}`

## Overview

Active-Call supports two template syntaxes that **don't conflict** and work at different stages for different purposes.

## Comparison Table

| Feature | `${VAR}` | `{{var}}` |
|---------|----------|-----------|
| **Purpose** | Environment variable substitution | Runtime variable substitution |
| **Timing** | Playbook load time (static) | During conversation (dynamic) |
| **Scope** | YAML configuration section | Prompt text section |
| **Source** | System environment variables | SIP Headers, set_var, state |
| **Engine** | Rust regex | MiniJinja template engine |
| **Typical Use** | API Keys, config parameters | Customer info, conversation state |

## Execution Order

```
┌─────────────────────────────────────────────────────────┐
│ 1. Load Playbook File                                    │
│    *.md file from disk                                   │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 2. Jinja2 Rendering (if variables parameter passed)     │
│    {{var}} → Runtime variable replacement                │
│    Used to dynamically generate playbook content (opt.)  │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 3. Split YAML and Prompt                                 │
│    Split by "---"                                        │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 4. Environment Variable Expansion (in YAML section)     │
│    ${VAR} → environment variable value                   │
│    ⚠️ Only affects YAML configuration                    │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 5. YAML Parsing                                          │
│    serde_yaml::from_str()                                │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 6. Prompt Section Preserved                              │
│    {{var}} kept in prompt, waiting for runtime replace   │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ 7. Call Starts - Runtime Variable Injection             │
│    - Extract SIP Headers → {{X-CID}}                     │
│    - LLM set_var → {{user_name}}                         │
│    - Replace {{var}} when rendering prompt               │
└─────────────────────────────────────────────────────────┘
```

## Detailed Explanation

### `${VAR}` - Environment Variables (Configuration Time)

**Timing**: When playbook loads, **before** YAML parsing  
**Location**: Only in YAML configuration section (between `---`)  
**Source**: System environment variables

```markdown
---
llm:
  apiKey: "${OPENAI_API_KEY}"    # ← Use ${VAR} here
  baseUrl: "${OPENAI_BASE_URL}"
  model: "${OPENAI_MODEL}"
tts:
  speed: ${TTS_SPEED}             # Numeric values work too
---
# Scene: main
Your prompt here
```

**Processing Flow**:
1. Read environment variable `OPENAI_API_KEY`
2. Replace `"${OPENAI_API_KEY}"` → `"sk-actual-key"`
3. YAML parser sees: `apiKey: "sk-actual-key"`

### `{{var}}` - Runtime Variables (Conversation Time)

**Timing**: During conversation, each time prompt is generated  
**Location**: Mainly in Prompt text section (after `---`)  
**Source**: SIP Headers, LLM set_var commands, state variables

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

Hello, {{X-Name}}!
Your customer ID is: {{X-CID}}
Conversation turn: {{turn_count}}
```

**Processing Flow**:
1. Call starts, extract SIP Header: `X-Name=John`
2. Generate prompt, replace `{{X-Name}}` → `John`
3. Different value for each call

## Real-World Examples

### Example 1: Using Both Together

```markdown
---
# Configuration: Use ${VAR} from environment variables
asr:
  provider: "${ASR_PROVIDER}"      # Env var: sensevoice
  language: "${ASR_LANGUAGE}"      # Env var: zh
  
tts:
  provider: "${TTS_PROVIDER}"      # Env var: supertonic
  speaker: "${TTS_SPEAKER}"        # Env var: F1
  
llm:
  provider: "${LLM_PROVIDER}"      # Env var: openai
  apiKey: "${OPENAI_API_KEY}"      # Env var: sk-xxx
  model: "${LLM_MODEL}"            # Env var: gpt-4o
  
sip:
  extract_headers:
    - "X-Customer-ID"
    - "X-Customer-Name"
    - "X-VIP-Level"
---

# Prompt: Use {{var}} for runtime replacement
# Scene: greeting

Hello, {{X-Customer-Name}}!
Your customer ID is: {{X-Customer-ID}}
You are our {{X-VIP-Level}} customer.

How can I help you today?
```

**Execution Result**:

1. **At load time (${VAR} replacement)**:
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

2. **At runtime ({{var}} replacement)**:
   ```
   Hello, John!
   Your customer ID is: C12345
   You are our Gold customer.
   ```

### Example 2: Using {{var}} in YAML - Not Recommended

```markdown
---
llm:
  apiKey: "{{api_key}}"  # ❌ Not recommended! This will fail
---
```

**Why it fails?**
- `{{api_key}}` exists when YAML is parsed
- YAML parser treats it as literal string `"{{api_key}}"`
- LLM config gets invalid API key

**Correct approach**:
```markdown
---
llm:
  apiKey: "${OPENAI_API_KEY}"  # ✅ Use environment variable
---
```

### Example 3: Using ${VAR} in Prompt - Will Be Kept

```markdown
---
llm:
  provider: "openai"
---
# Scene: main

Hello, I'm using model: ${LLM_MODEL}
```

**Result**:
- If `LLM_MODEL` env var exists: `"Hello, I'm using model: gpt-4o"`
- If not exists: `"Hello, I'm using model: ${LLM_MODEL}"` (kept as-is)

## Variable Source Comparison

### `${VAR}` Sources

```bash
# From environment variables
export OPENAI_API_KEY=sk-xxx
export TTS_SPEED=1.2
export ASR_LANGUAGE=zh

# In Docker
docker run -e OPENAI_API_KEY=sk-xxx ...

# Or .env file
OPENAI_API_KEY=sk-xxx
TTS_SPEED=1.2
```

### `{{var}}` Sources

```markdown
# 1. SIP Headers
sip:
  extract_headers:
    - "X-CID"         # → {{X-CID}}

# 2. LLM set_var command
<set_var key="user_name" value="John" />
# → {{user_name}}

# 3. HTTP response
<http url="..." method="GET" set_var="api_response" />
# → {{api_response}}

# 4. System built-ins
{{turn_count}}      # Conversation turn count
{{summary}}         # Conversation summary
{{session_id}}      # Unique session identifier
{{call_type}}       # Call type (sip/websocket/webrtc/b2bua)
{{caller}}          # Caller SIP URI (SIP only)
{{callee}}          # Callee SIP URI (SIP only)
{{start_time}}      # Call start time (RFC 3339)
```

## Common Questions

### Q1: Can I use {{var}} in YAML?

**A**: Technically yes, but not recommended.

```markdown
---
# Pass variables parameter before parse()
llm:
  model: "{{model_name}}"  # Works, but confusing
---
```

This gets replaced during **Jinja2 rendering stage** (step 2), before environment variable expansion. But this makes config complex - use `${VAR}` instead.

### Q2: Can I use ${VAR} in Prompt?

**A**: Yes, but it gets replaced at playbook load time to a fixed value.

```markdown
---
llm:
  provider: "openai"
---
# Scene: main

Hello! API Endpoint: ${OPENAI_BASE_URL}
```

This `${OPENAI_BASE_URL}` gets replaced at load time, all calls see the same value.  
If you want dynamic values, use `{{var}}`.

### Q3: Can they be nested?

**A**: No direct nesting, but can be combined.

```markdown
# ❌ Not supported
apiKey: "${OPENAI_${ENV}_KEY}"  # Won't expand

# ✅ Correct approach
export OPENAI_DEV_KEY=sk-dev
export API_KEY_VAR=OPENAI_DEV_KEY

# Then in shell:
export FINAL_KEY=$(eval echo \$$API_KEY_VAR)
```

### Q4: What's the precedence?

Execution order determines precedence:

1. **Jinja2** `{{var}}` - if variables passed in parse()
2. **Environment vars** `${VAR}` - before YAML parsing
3. **Runtime** `{{var}}` - during conversation

## Best Practices

### ✅ Recommended

```markdown
---
# Config: Static values use ${VAR}
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

# Prompt: Dynamic values use {{var}}
# Scene: main

Hello, {{X-Customer-Name}}!
Customer ID: {{X-Customer-ID}}

<set_var key="greeting_done" value="true" />
```

### ❌ Avoid

```markdown
---
# ❌ Runtime variables in YAML
llm:
  apiKey: "{{runtime_api_key}}"  # Will fail

# ❌ Mixing syntaxes
tts:
  speaker: "${TTS_SPEAKER}}"  # Extra }
---

# ❌ Expecting dynamic config in Prompt
# Scene: main
Current model: ${LLM_MODEL}  # Static, fixed at load time
```

## Summary

| Scenario | Use | Reason |
|----------|-----|--------|
| API Keys | `${OPENAI_API_KEY}` | Sensitive data from env vars |
| Model config | `${LLM_MODEL}` | Config param, set at deploy time |
| TTS params | `${TTS_SPEED}` | Static config, determined at load |
| Customer info | `{{X-Customer-Name}}` | Different per call |
| Conversation state | `{{user_confirmed}}` | Changes at runtime |
| HTTP response | `{{api_response}}` | Retrieved at runtime |
| Session info | `{{session_id}}` | Built-in auto-injected |
| Call type | `{{call_type}}` | Built-in auto-injected |

**Remember**:
- **`${VAR}`** = Configuration time (static) = Environment variables
- **`{{var}}`** = Runtime (dynamic) = Conversation state

They complement each other without conflict! 🎯

---

## Additional Resources

- [Playbook Advanced Features](playbook_advanced_features.en.md) - Complete feature guide
- [Environment Variables Example](../config/playbook/env_vars_example.md) - Detailed env var usage
- [Configuration Guide](config_guide.en.md) - Full configuration reference
- [README](../README.md) - Getting started guide
