# Dialogue System Architecture

## Overview

The dialogue system in `active-call` is designed to separate the **Execution Engine** from the **Dialogue Logic** (Brain). This allows for flexible implementation of different dialogue strategies (LLM-based, NLP-based, Rule-based) without changing the core call handling logic.

## Core Components

### 1. PlaybookRunner (The Engine)
- **Role**: Orchestrates the conversation.
- **Responsibility**:
  - Manages the lifecycle of the `DialogueHandler`.
  - Subscribes to `SessionEvent`s from `ActiveCall`.
  - Dispatches events to the `DialogueHandler`.
  - Executes `Command`s returned by the handler (e.g., sending TTS, hanging up).

### 2. DialogueHandler (The Brain)
- **Role**: Abstract interface for dialogue management.
- **Trait Definition**:
  ```rust
  #[async_trait]
  pub trait DialogueHandler: Send + Sync {
      /// Called when session starts. Returns initial commands (e.g. greeting).
      async fn on_start(&mut self) -> Result<Vec<Command>>;

      /// Called when a session event occurs (ASR, VAD, Silence, etc.).
      async fn on_event(&mut self, event: &SessionEvent) -> Result<Vec<Command>>;
  }
  ```

### 3. LlmHandler (Implementation)
- **Role**: A concrete implementation of `DialogueHandler` using LLMs (e.g., OpenAI).
- **Features**:
  - **Context Management**: Maintains conversation history (`ChatMessage`).
  - **Control Logic**: Parses LLM responses to control flow (interruption, timeouts).
  - **State Tracking**: Tracks if the bot `is_speaking` to handle interruptions.

## Workflow

### 1. Session Start
1. `PlaybookRunner` initializes.
2. Calls `handler.on_start()`.
3. `LlmHandler` generates a greeting (static or via LLM) and returns `Command::Tts`.
4. `PlaybookRunner` executes the command.

### 2. Event Loop
The system listens for events and reacts:

#### A. User Speaks (`AsrFinal`)
1. User finishes a sentence.
2. `LlmHandler` receives `AsrFinal`.
3. Adds user input to history.
4. Calls LLM API.
5. Returns `Command::Tts` with the response.

#### B. Interruption (`AsrDelta` / `Speaking`)
1. User starts speaking while bot is speaking.
2. `LlmHandler` checks `is_speaking` and `allow_interrupt`.
3. If allowed, returns `Command::Interrupt` to stop TTS immediately.

#### C. Silence / Timeout (`Silence`)
1. User stays silent for `wait_input_timeout` (configured in previous TTS command).
2. `ActiveCall` emits `SessionEvent::Silence`.
3. `LlmHandler` triggers a "Follow-up" logic (e.g., asking "Are you still there?").
4. Returns new `Command::Tts`.

## Advanced Features

### EOU (End of Utterance)
- Configured via `PlaybookConfig.eou`.
- Determines when the engine considers the user has finished speaking.
- **Use Cases**:
  - **Number Collection**: Allow longer pauses between digits.
  - **Semantic Turn-taking**: Wait for complete sentences.

### Control Parameters (JSON Mode)
The LLM can return structured JSON to control the call flow:
```json
{
  "text": "Please say your ID number.",
  "allow_interrupt": false,
  "wait_input_timeout": 20000
}
```
- `allow_interrupt`: Disables interruption during critical prompts.
- `wait_input_timeout`: Sets how long to wait for user input before triggering `Silence`.

## Streaming Support (Design Consideration)

Currently, `LlmHandler` waits for the full LLM response before sending TTS. To support **Streaming (Low Latency)**:

1. **LLM Streaming**: The `call_llm` method should use the provider's streaming API (SSE).
2. **TTS Streaming**:
   - `ActiveCall` supports `Command::Tts { streaming: Some(true), ... }`.
   - The handler needs to send an initial streaming command.
   - Subsequent chunks of text should be sent as they arrive from the LLM.
3. **Architecture Update**:
   - `DialogueHandler::on_event` might need to return a `Stream` of commands, or the handler needs to spawn a background task to push chunks to `ActiveCall`.
   - Alternatively, `Command::Tts` could accept a `Receiver<String>` (channel) for text input.

### Proposed Streaming Flow
1. `on_event` triggers LLM stream.
2. Handler immediately returns `Command::Tts { streaming: true, ... }`.
3. Handler spawns a task to read LLM stream and send `Command::Tts { text: chunk, ... }` (or a specialized `AudioData` command) to the call.
