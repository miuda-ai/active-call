# Environment Variables Example

This example demonstrates universal environment variable support in Playbook configurations.

## Overview

**All configuration fields** support `${VAR_NAME}` syntax, allowing you to:
- Keep sensitive data out of version control
- Share configurations across environments (dev/staging/prod)
- Dynamically configure providers and models

## Example Configuration

```markdown
---
asr:
  provider: "${ASR_PROVIDER}"        # e.g., "sensevoice", "tencent", "aliyun"
  language: "${ASR_LANGUAGE}"        # e.g., "zh", "en", "auto"
  app_id: "${ASR_APP_ID}"            # For cloud providers
  secret_id: "${ASR_SECRET_ID}"
  secret_key: "${ASR_SECRET_KEY}"

tts:
  provider: "${TTS_PROVIDER}"        # e.g., "supertonic", "cosyvoice"
  speaker: "${TTS_SPEAKER}"          # e.g., "F1", "M1"
  speed: ${TTS_SPEED}                # Numeric values: 0.8, 1.0, 1.2
  language: "${TTS_LANGUAGE}"        # e.g., "en", "zh"

llm:
  provider: "${LLM_PROVIDER}"        # e.g., "openai", "azure", "dashscope"
  model: "${LLM_MODEL}"              # e.g., "gpt-4o", "gpt-4o-mini"
  apiKey: "${LLM_API_KEY}"
  baseUrl: "${LLM_BASE_URL}"
  temperature: ${LLM_TEMPERATURE}    # Numeric: 0.0 to 2.0
  max_tokens: ${LLM_MAX_TOKENS}      # Integer values

vad:
  provider: "${VAD_PROVIDER}"        # e.g., "silero", "webrtc"
  sensitivity: ${VAD_SENSITIVITY}    # Numeric: 0.0 to 1.0

posthook:
  url: "${WEBHOOK_URL}"              # e.g., "https://api.example.com/webhooks"
  summary: "${SUMMARY_TYPE}"         # e.g., "auto", "brief", "detailed"
---

# Scene: main

You are ${AGENT_NAME}, a helpful ${AGENT_ROLE} assistant.
Your company is ${COMPANY_NAME}.

When helping users, remember to:
- Be friendly and professional
- Ask clarifying questions
- Provide accurate information about ${PRODUCT_NAME}
```

## Environment File (.env)

```bash
# ASR Configuration
ASR_PROVIDER=sensevoice
ASR_LANGUAGE=zh

# TTS Configuration
TTS_PROVIDER=supertonic
TTS_SPEAKER=F1
TTS_SPEED=1.0
TTS_LANGUAGE=zh

# LLM Configuration
LLM_PROVIDER=openai
LLM_MODEL=gpt-4o-mini
LLM_API_KEY=sk-xxx
LLM_BASE_URL=https://api.openai.com/v1
LLM_TEMPERATURE=0.7
LLM_MAX_TOKENS=4096

# VAD Configuration
VAD_PROVIDER=silero
VAD_SENSITIVITY=0.5

# Webhook
WEBHOOK_URL=https://api.example.com/call-summary

# Agent Personality
AGENT_NAME=Alice
AGENT_ROLE=customer support
COMPANY_NAME=TechCorp
PRODUCT_NAME=SmartWidget Pro
SUMMARY_TYPE=detailed
```

## Benefits

### 1. Security
- API keys never committed to git
- Rotate credentials without code changes
- Different keys per environment

### 2. Flexibility
- Same playbook, different configurations
- Easy A/B testing of models
- Quick provider switching

### 3. Multi-Environment
```bash
# Development
export LLM_PROVIDER=openai
export LLM_MODEL=gpt-4o-mini

# Production
export LLM_PROVIDER=azure
export LLM_MODEL=gpt-4o
export LLM_BASE_URL=https://your-resource.openai.azure.com/
```

### 4. Dynamic Numeric Values
```yaml
tts:
  speed: ${TTS_SPEED}              # 1.0
  
llm:
  temperature: ${LLM_TEMPERATURE}   # 0.7
  max_tokens: ${LLM_MAX_TOKENS}     # 4096

vad:
  sensitivity: ${VAD_SENSITIVITY}   # 0.5
```

## Docker Usage

Mount environment file:

```bash
docker run -d \
  --net host \
  --env-file .env \
  -v $(pwd)/config:/app/config \
  active-call:latest
```

Or pass individual variables:

```bash
docker run -d \
  --net host \
  -e LLM_PROVIDER=openai \
  -e LLM_MODEL=gpt-4o-mini \
  -e LLM_API_KEY=${OPENAI_API_KEY} \
  -v $(pwd)/config:/app/config \
  active-call:latest
```

## Fallback Behavior

If an environment variable is not set:
- The `${VAR_NAME}` placeholder is **kept as-is** in the string
- YAML parser may fail if required field is invalid
- Best practice: Always set required variables

Example:
```bash
# Variable not set
${UNDEFINED_VAR}  →  "${UNDEFINED_VAR}" (kept as-is)

# Variable set
export MY_VAR=hello
${MY_VAR}  →  "hello"
```

## Advanced Pattern: Environment-Specific Configs

### playbook_dev.md
```markdown
---
llm:
  provider: "openai"
  model: "gpt-4o-mini"      # Cheaper model for dev
  apiKey: "${DEV_API_KEY}"
---
# Testing scenario...
```

### playbook_prod.md
```markdown
---
llm:
  provider: "azure"
  model: "gpt-4o"           # Production model
  apiKey: "${PROD_API_KEY}"
  baseUrl: "${PROD_BASE_URL}"
---
# Production scenario...
```

## Testing

You can test variable expansion:

```bash
# Set test variables
export ASR_LANGUAGE=en
export TTS_SPEAKER=M1
export LLM_MODEL=gpt-4o

# Start active-call
./active-call --config active-call.toml

# Variables will be expanded when playbook is loaded
```

## Best Practices

1. **Use Descriptive Names**: `OPENAI_API_KEY` better than `KEY1`
2. **Group by Service**: Prefix with service name (e.g., `ASR_*`, `TTS_*`)
3. **Document Required Vars**: List all required env vars in README
4. **Provide Defaults**: Use fallback values in code when appropriate
5. **Validate on Startup**: Check critical env vars exist before running

## See Also

- [Simple CRM Example](simple_crm.md) - Basic HTTP integration
- [Webhook Example](webhook_example.md) - External API calls
- [Advanced Features Guide](../../docs/playbook_advanced_features.md) - Complete reference
