---
asr:
  provider: "sensevoice"
  extra:
    silence_threshold: "0.05" # only for sensevoice 0.03 - 0.1
  # sensevoice is local provider
  #language: "auto"
  ## see `TranscriptionOption` options
llm:
  provider: "aliyun"
  # model: "qwen-plus"
  # model: # .env OPENAI_MODEL
  # baseUrl: # .env OPENAI_BASE_URL
  # apiKey: # .env OPENAI_API_KEY
tts:
  provider: "supertonic"  # aliyun, tencent, supertonic
  # supertonic is local provider
  speaker: "F1"           # M1, M2, F1, F2
  speed: 1.0
  language: "en"          # en, ko, es, pt, fr
  # provider: "msedge"
  # speaker: "zh-CN-XiaoxiaoNeural"
  # see `SynthesisOption` options

vad:
  provider: "silero"
denoise: true
greeting: "Hello, how can i help you?"
interruption:
  strategy: "both"
followup:
  timeout: 10000
  max: 2
recorder:
  recorderFile: "hello_{id}.wav"
ambiance:
 path: "./config/office.wav"
 duckLevel: 0.1
 normalLevel: 0.5
 transitionSpeed: 0.1
---
# Role and Purpose
You are an intelligent, polite AI assistant. Your goal is to help users with their inquiries efficiently.

# Tool Usage
- When the user expresses a desire to end the conversation (e.g., "goodbye", "hang up", "I'm done"), you MUST provide a polite closing statement AND output `<hangup/>`.

# Example Response for Hanging Up:
Goodbye! <hangup/>
---
