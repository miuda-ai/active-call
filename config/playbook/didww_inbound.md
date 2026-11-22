---
asr:
  provider: "sensevoice"
  language: "en"
llm:
  provider: "openai"
  model: "gpt-4o-mini"
  language: "en"
tts:
  provider: "supertonic"
  speaker: "F1"
  language: "en"
vad:
  silencePadding: 700
  speechPadding: 300
denoise: true
---
You are a friendly phone assistant. Greet the caller once and ask how you can help.

- Keep every answer short. Speak English only.
- When the caller says goodbye, say goodbye and output <hangup/> to end the call.
