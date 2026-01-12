use crate::ReferOption;
use crate::call::Command;
use crate::event::SessionEvent;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{info, warn};

static RE_HANGUP: Lazy<Regex> = Lazy::new(|| Regex::new(r"<hangup\s*/>").unwrap());
static RE_REFER: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<refer\s+to="([^"]+)"\s*/>"#).unwrap());
static RE_SENTENCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)[.!?。！？\n]\s*").unwrap());

use super::InterruptionStrategy;
use super::LlmConfig;
use super::dialogue::DialogueHandler;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

const MAX_RAG_ATTEMPTS: usize = 3;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn call(&self, config: &LlmConfig, history: &[ChatMessage]) -> Result<String>;
    async fn call_stream(
        &self,
        config: &LlmConfig,
        history: &[ChatMessage],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>>;
}

struct DefaultLlmProvider {
    client: Client,
}

impl DefaultLlmProvider {
    fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }
}

#[async_trait]
impl LlmProvider for DefaultLlmProvider {
    async fn call(&self, config: &LlmConfig, history: &[ChatMessage]) -> Result<String> {
        let mut url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-4-turbo".to_string());
        let api_key = config.api_key.clone().unwrap_or_default();

        if !url.ends_with("/chat/completions") {
            url = format!("{}/chat/completions", url.trim_end_matches('/'));
        }

        let body = json!({
            "model": model,
            "messages": history,
        });

        let res = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await?;

        if !res.status().is_success() {
            return Err(anyhow!("LLM request failed: {}", res.status()));
        }

        let json: serde_json::Value = res.json().await?;
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid LLM response"))?
            .to_string();

        Ok(content)
    }

    async fn call_stream(
        &self,
        config: &LlmConfig,
        history: &[ChatMessage],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let mut url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-4-turbo".to_string());
        let api_key = config.api_key.clone().unwrap_or_default();

        if !url.ends_with("/chat/completions") {
            url = format!("{}/chat/completions", url.trim_end_matches('/'));
        }

        let body = json!({
            "model": model,
            "messages": history,
            "stream": true,
        });

        let res = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await?;

        if !res.status().is_success() {
            return Err(anyhow!("LLM request failed: {}", res.status()));
        }

        let stream = res.bytes_stream();
        let s = async_stream::stream! {
            let mut buffer = String::new();
            for await chunk in stream {
                match chunk {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        buffer.push_str(&text);

                        while let Some(line_end) = buffer.find('\n') {
                            let line = buffer[..line_end].trim();
                            if line.starts_with("data:") {
                                let data = &line[5..].trim();
                                if *data == "[DONE]" {
                                    break;
                                }
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                    if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                                        yield Ok(content.to_string());
                                    }
                                }
                            }
                            buffer.drain(..=line_end);
                        }
                    }
                    Err(e) => yield Err(anyhow!(e)),
                }
            }
        };

        Ok(Box::pin(s))
    }
}

#[async_trait]
pub trait RagRetriever: Send + Sync {
    async fn retrieve(&self, query: &str) -> Result<String>;
}

struct NoopRagRetriever;

#[async_trait]
impl RagRetriever for NoopRagRetriever {
    async fn retrieve(&self, _query: &str) -> Result<String> {
        Ok(String::new())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StructuredResponse {
    text: Option<String>,
    wait_input_timeout: Option<u32>,
    tools: Option<Vec<ToolInvocation>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "name", rename_all = "lowercase")]
enum ToolInvocation {
    #[serde(rename_all = "camelCase")]
    Hangup {
        reason: Option<String>,
        initiator: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Refer {
        caller: String,
        callee: String,
        options: Option<ReferOption>,
    },
    #[serde(rename_all = "camelCase")]
    Rag {
        query: String,
        source: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Accept { options: Option<crate::CallOption> },
    #[serde(rename_all = "camelCase")]
    Reject {
        reason: Option<String>,
        code: Option<u32>,
    },
}

pub struct LlmHandler {
    config: LlmConfig,
    history: Vec<ChatMessage>,
    provider: Arc<dyn LlmProvider>,
    rag_retriever: Arc<dyn RagRetriever>,
    is_speaking: bool,
    event_sender: Option<crate::event::EventSender>,
    last_asr_final_at: Option<std::time::Instant>,
    interruption: InterruptionStrategy,
    call: Option<crate::call::ActiveCallRef>,
}

impl LlmHandler {
    pub fn new(config: LlmConfig, interruption: InterruptionStrategy) -> Self {
        Self::with_provider(
            config,
            Arc::new(DefaultLlmProvider::new()),
            Arc::new(NoopRagRetriever),
            interruption,
        )
    }

    pub fn with_provider(
        config: LlmConfig,
        provider: Arc<dyn LlmProvider>,
        rag_retriever: Arc<dyn RagRetriever>,
        interruption: InterruptionStrategy,
    ) -> Self {
        let mut history = Vec::new();
        let prompt = config.prompt.clone().unwrap_or_default();
        let system_prompt = format!(
            "{}\n\n\
            Tool usage instructions:\n\
            - To hang up the call, use: <hangup/>\n\
            - To transfer the call, use: <refer to=\"sip:xxxx\"/>\n\
            Use these XML-like tags instead of JSON. Efficiency is key. \
            Output your response in short sentences. Each sentence will be played as soon as it is finished.",
            prompt
        );

        history.push(ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        });

        Self {
            config,
            history,
            provider,
            rag_retriever,
            is_speaking: false,
            event_sender: None,
            last_asr_final_at: None,
            interruption,
            call: None,
        }
    }

    pub fn set_call(&mut self, call: crate::call::ActiveCallRef) {
        self.call = Some(call);
    }

    pub fn set_event_sender(&mut self, sender: crate::event::EventSender) {
        self.event_sender = Some(sender);
    }

    fn send_debug_event(&self, key: &str, data: serde_json::Value) {
        if let Some(sender) = &self.event_sender {
            let event = crate::event::SessionEvent::Metrics {
                timestamp: crate::media::get_timestamp(),
                key: key.to_string(),
                duration: 0,
                data,
            };
            let _ = sender.send(event);
        }
    }

    async fn call_llm(&self) -> Result<String> {
        self.provider.call(&self.config, &self.history).await
    }

    fn create_tts_command(
        &self,
        text: String,
        wait_input_timeout: Option<u32>,
        auto_hangup: Option<bool>,
    ) -> Command {
        let timeout = wait_input_timeout.unwrap_or(10000);
        Command::Tts {
            text,
            speaker: None,
            play_id: Some(uuid::Uuid::new_v4().to_string()),
            auto_hangup,
            streaming: None,
            end_of_stream: None,
            option: None,
            wait_input_timeout: Some(timeout),
            base64: None,
        }
    }

    async fn generate_response(&mut self) -> Result<Vec<Command>> {
        // Send debug event - LLM call started
        self.send_debug_event(
            "llm_call_start",
            json!({
                "history_length": self.history.len(),
            }),
        );

        let play_id = uuid::Uuid::new_v4().to_string();
        let mut stream = self
            .provider
            .call_stream(&self.config, &self.history)
            .await?;

        let mut full_content = String::new();
        let mut buffer = String::new();
        let mut commands = Vec::new();
        let mut is_json_mode = false;
        let mut checked_json_mode = false;

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    warn!("LLM stream error: {}", e);
                    break;
                }
            };

            full_content.push_str(&chunk);
            buffer.push_str(&chunk);

            if !checked_json_mode {
                let trimmed = full_content.trim();
                if !trimmed.is_empty() {
                    if trimmed.starts_with('{') || trimmed.starts_with('`') {
                        is_json_mode = true;
                    }
                    checked_json_mode = true;
                }
            }

            if checked_json_mode && !is_json_mode {
                let extracted = self.extract_streaming_commands(&mut buffer, &play_id, false);
                for cmd in extracted {
                    if let Some(call) = &self.call {
                        let _ = call.enqueue_command(cmd).await;
                    } else {
                        commands.push(cmd);
                    }
                }
            }
        }

        // Send debug event - LLM response received
        self.send_debug_event(
            "llm_response",
            json!({
                "response": full_content,
                "is_json_mode": is_json_mode,
            }),
        );

        if is_json_mode {
            self.interpret_response(full_content).await
        } else {
            let extracted = self.extract_streaming_commands(&mut buffer, &play_id, true);
            for cmd in extracted {
                if let Some(call) = &self.call {
                    let _ = call.enqueue_command(cmd).await;
                } else {
                    commands.push(cmd);
                }
            }
            if !full_content.trim().is_empty() {
                self.history.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: full_content,
                });
                self.is_speaking = true;
            }
            Ok(commands)
        }
    }

    fn extract_streaming_commands(
        &self,
        buffer: &mut String,
        play_id: &str,
        is_final: bool,
    ) -> Vec<Command> {
        let mut commands = Vec::new();

        loop {
            let hangup_pos = RE_HANGUP.find(buffer);
            let refer_pos = RE_REFER.captures(buffer);
            let sentence_pos = RE_SENTENCE.find(buffer);

            // Find the first occurrence
            let mut positions = Vec::new();
            if let Some(m) = hangup_pos {
                positions.push((m.start(), 0));
            }
            if let Some(caps) = &refer_pos {
                positions.push((caps.get(0).unwrap().start(), 1));
            }
            if let Some(m) = sentence_pos {
                positions.push((m.start(), 2));
            }

            positions.sort_by_key(|p| p.0);

            if let Some((pos, kind)) = positions.first() {
                let pos = *pos;
                match kind {
                    0 => {
                        // Hangup
                        let prefix = buffer[..pos].to_string();
                        if !prefix.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                prefix,
                                play_id.to_string(),
                                None,
                            ));
                        }
                        commands.push(Command::Hangup {
                            reason: None,
                            initiator: None,
                        });
                        buffer.drain(..RE_HANGUP.find(buffer).unwrap().end());
                        // Stop after hangup
                        return commands;
                    }
                    1 => {
                        // Refer
                        let caps = RE_REFER.captures(buffer).unwrap();
                        let mat = caps.get(0).unwrap();
                        let callee = caps.get(1).unwrap().as_str().to_string();

                        let prefix = buffer[..pos].to_string();
                        if !prefix.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                prefix,
                                play_id.to_string(),
                                None,
                            ));
                        }
                        commands.push(Command::Refer {
                            caller: String::new(),
                            callee,
                            options: None,
                        });
                        buffer.drain(..mat.end());
                    }
                    2 => {
                        // Sentence
                        let mat = sentence_pos.unwrap();
                        let sentence = buffer[..mat.end()].to_string();
                        if !sentence.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                sentence,
                                play_id.to_string(),
                                None,
                            ));
                        }
                        buffer.drain(..mat.end());
                    }
                    _ => unreachable!(),
                }
            } else {
                break;
            }
        }

        if is_final {
            let remaining = buffer.trim().to_string();
            if !remaining.is_empty() {
                commands.push(self.create_tts_command_with_id(
                    remaining,
                    play_id.to_string(),
                    None,
                ));
            }
            buffer.clear();
        }

        commands
    }

    fn create_tts_command_with_id(
        &self,
        text: String,
        play_id: String,
        auto_hangup: Option<bool>,
    ) -> Command {
        Command::Tts {
            text,
            speaker: None,
            play_id: Some(play_id),
            auto_hangup,
            streaming: Some(true),
            end_of_stream: None,
            option: None,
            wait_input_timeout: Some(10000),
            base64: None,
        }
    }

    async fn interpret_response(&mut self, initial: String) -> Result<Vec<Command>> {
        let mut tool_commands = Vec::new();
        let mut wait_input_timeout = None;
        let mut attempts = 0;
        let final_text: Option<String>;
        let mut raw = initial;

        loop {
            attempts += 1;
            let mut rerun_for_rag = false;

            if let Some(structured) = parse_structured_response(&raw) {
                if wait_input_timeout.is_none() {
                    wait_input_timeout = structured.wait_input_timeout;
                }

                if let Some(tools) = structured.tools {
                    for tool in tools {
                        match tool {
                            ToolInvocation::Hangup {
                                ref reason,
                                ref initiator,
                            } => {
                                // Send debug event
                                self.send_debug_event(
                                    "tool_invocation",
                                    json!({
                                        "tool": "Hangup",
                                        "params": {
                                            "reason": reason,
                                            "initiator": initiator,
                                        }
                                    }),
                                );
                                tool_commands.push(Command::Hangup {
                                    reason: reason.clone(),
                                    initiator: initiator.clone(),
                                });
                            }
                            ToolInvocation::Refer {
                                ref caller,
                                ref callee,
                                ref options,
                            } => {
                                // Send debug event
                                self.send_debug_event(
                                    "tool_invocation",
                                    json!({
                                        "tool": "Refer",
                                        "params": {
                                            "caller": caller,
                                            "callee": callee,
                                        }
                                    }),
                                );
                                tool_commands.push(Command::Refer {
                                    caller: caller.clone(),
                                    callee: callee.clone(),
                                    options: options.clone(),
                                });
                            }
                            ToolInvocation::Rag {
                                ref query,
                                ref source,
                            } => {
                                // Send debug event - RAG query started
                                self.send_debug_event(
                                    "tool_invocation",
                                    json!({
                                        "tool": "Rag",
                                        "params": {
                                            "query": query,
                                            "source": source,
                                        }
                                    }),
                                );

                                let rag_result = self.rag_retriever.retrieve(&query).await?;

                                // Send debug event - RAG result
                                self.send_debug_event(
                                    "rag_result",
                                    json!({
                                        "query": query,
                                        "result": rag_result,
                                    }),
                                );

                                let summary = if let Some(source) = source {
                                    format!("[{}] {}", source, rag_result)
                                } else {
                                    rag_result
                                };
                                self.history.push(ChatMessage {
                                    role: "system".to_string(),
                                    content: format!("RAG result for {}: {}", query, summary),
                                });
                                rerun_for_rag = true;
                            }
                            ToolInvocation::Accept { ref options } => {
                                self.send_debug_event(
                                    "tool_invocation",
                                    json!({
                                        "tool": "Accept",
                                    }),
                                );
                                tool_commands.push(Command::Accept {
                                    option: options.clone().unwrap_or_default(),
                                });
                            }
                            ToolInvocation::Reject { ref reason, code } => {
                                self.send_debug_event(
                                    "tool_invocation",
                                    json!({
                                        "tool": "Reject",
                                        "params": {
                                            "reason": reason,
                                            "code": code,
                                        }
                                    }),
                                );
                                tool_commands.push(Command::Reject {
                                    reason: reason
                                        .clone()
                                        .unwrap_or_else(|| "Rejected by agent".to_string()),
                                    code,
                                });
                            }
                        }
                    }
                }

                if rerun_for_rag {
                    if attempts >= MAX_RAG_ATTEMPTS {
                        warn!("Reached RAG iteration limit, using last response");
                        final_text = structured.text.or_else(|| Some(raw.clone()));
                        break;
                    }
                    raw = self.call_llm().await?;
                    continue;
                }

                final_text = structured.text;
                break;
            }

            final_text = Some(raw.clone());
            break;
        }

        let mut commands = Vec::new();

        // Check if any tool invocation is a hangup
        let mut has_hangup = false;
        for tool in &tool_commands {
            if matches!(tool, Command::Hangup { .. }) {
                has_hangup = true;
                break;
            }
        }

        if let Some(text) = final_text {
            if !text.trim().is_empty() {
                self.history.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: text.clone(),
                });
                self.is_speaking = true;

                // If we have text and a hangup command, use auto_hangup on the TTS command
                // and remove the separate hangup command.
                if has_hangup {
                    commands.push(self.create_tts_command(text, wait_input_timeout, Some(true)));
                    tool_commands.retain(|c| !matches!(c, Command::Hangup { .. }));
                } else {
                    commands.push(self.create_tts_command(text, wait_input_timeout, None));
                }
            }
        }

        commands.extend(tool_commands);

        Ok(commands)
    }
}

fn parse_structured_response(raw: &str) -> Option<StructuredResponse> {
    let payload = extract_json_block(raw)?;
    serde_json::from_str(payload).ok()
}

fn extract_json_block(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('`') {
        if let Some(end) = trimmed.rfind("```") {
            if end <= 3 {
                return None;
            }
            let mut inner = &trimmed[3..end];
            inner = inner.trim();
            if inner.to_lowercase().starts_with("json") {
                if let Some(newline) = inner.find('\n') {
                    inner = inner[newline + 1..].trim();
                } else if inner.len() > 4 {
                    inner = inner[4..].trim();
                } else {
                    inner = inner.trim();
                }
            }
            return Some(inner);
        }
    } else if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(trimmed);
    }
    None
}

#[async_trait]
impl DialogueHandler for LlmHandler {
    async fn on_start(&mut self) -> Result<Vec<Command>> {
        if let Some(greeting) = &self.config.greeting {
            self.is_speaking = true;
            return Ok(vec![self.create_tts_command(greeting.clone(), None, None)]);
        }

        self.generate_response().await
    }

    async fn on_event(&mut self, event: &SessionEvent) -> Result<Vec<Command>> {
        match event {
            SessionEvent::AsrFinal { text, .. } => {
                if text.trim().is_empty() {
                    return Ok(vec![]);
                }

                self.last_asr_final_at = Some(std::time::Instant::now());
                self.is_speaking = false;

                self.history.push(ChatMessage {
                    role: "user".to_string(),
                    content: text.clone(),
                });

                self.generate_response().await
            }

            SessionEvent::AsrDelta { .. } | SessionEvent::Speaking { .. } => {
                let should_interrupt = match (self.interruption, event) {
                    (InterruptionStrategy::None, _) => false,
                    (InterruptionStrategy::Vad, SessionEvent::Speaking { .. }) => true,
                    (InterruptionStrategy::Asr, SessionEvent::AsrDelta { .. }) => true,
                    (InterruptionStrategy::Both, _) => true,
                    _ => false,
                };

                if self.is_speaking && should_interrupt {
                    // Ignore interruptions that occur very shortly after the last ASR final.
                    // This prevents stale VAD events from cancelling the newly generated response.
                    if let Some(last_final) = self.last_asr_final_at {
                        if last_final.elapsed().as_millis() < 800 {
                            tracing::debug!("Ignoring interruption event too close to AsrFinal");
                            return Ok(vec![]);
                        }
                    }

                    info!("Interruption detected, stopping playback");
                    self.is_speaking = false;
                    return Ok(vec![Command::Interrupt {
                        graceful: Some(true),
                    }]);
                }
                Ok(vec![])
            }

            SessionEvent::Silence { .. } => {
                info!("Silence timeout detected, triggering follow-up");
                self.generate_response().await
            }

            SessionEvent::TrackEnd { .. } => {
                self.is_speaking = false;
                Ok(vec![])
            }

            _ => Ok(vec![]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SessionEvent;
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct TestProvider {
        responses: Mutex<VecDeque<String>>,
    }

    impl TestProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for TestProvider {
        async fn call(&self, _config: &LlmConfig, _history: &[ChatMessage]) -> Result<String> {
            let mut guard = self.responses.lock().unwrap();
            guard
                .pop_front()
                .ok_or_else(|| anyhow!("Test provider ran out of responses"))
        }

        async fn call_stream(
            &self,
            _config: &LlmConfig,
            _history: &[ChatMessage],
        ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
            let response = self.call(_config, _history).await?;
            let s = async_stream::stream! {
                yield Ok(response);
            };
            Ok(Box::pin(s))
        }
    }

    struct RecordingRag {
        queries: Mutex<Vec<String>>,
    }

    impl RecordingRag {
        fn new() -> Self {
            Self {
                queries: Mutex::new(Vec::new()),
            }
        }

        fn recorded_queries(&self) -> Vec<String> {
            self.queries.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RagRetriever for RecordingRag {
        async fn retrieve(&self, query: &str) -> Result<String> {
            self.queries.lock().unwrap().push(query.to_string());
            Ok(format!("retrieved {}", query))
        }
    }

    #[tokio::test]
    async fn handler_applies_tool_instructions() -> Result<()> {
        let response = r#"{
            "text": "Goodbye",
            "waitInputTimeout": 15000,
            "tools": [
                {"name": "hangup", "reason": "done", "initiator": "agent"},
                {"name": "refer", "caller": "sip:bot", "callee": "sip:lead"}
            ]
        }"#;

        let provider = Arc::new(TestProvider::new(vec![response.to_string()]));
        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(NoopRagRetriever),
            InterruptionStrategy::Both,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "track-1".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hello".to_string(),
        };

        let commands = handler.on_event(&event).await?;
        assert!(matches!(
            commands.get(0),
            Some(Command::Tts {
                text,
                wait_input_timeout: Some(15000),
                auto_hangup: Some(true),
                ..
            }) if text == "Goodbye"
        ));
        assert!(commands.iter().any(|cmd| matches!(
            cmd,
            Command::Refer {
                caller,
                callee,
                ..
            } if caller == "sip:bot" && callee == "sip:lead"
        )));

        Ok(())
    }

    #[tokio::test]
    async fn handler_requeries_after_rag() -> Result<()> {
        let rag_instruction = r#"{"tools": [{"name": "rag", "query": "policy"}]}"#;
        let provider = Arc::new(TestProvider::new(vec![
            rag_instruction.to_string(),
            "Final answer".to_string(),
        ]));
        let rag = Arc::new(RecordingRag::new());
        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            rag.clone(),
            InterruptionStrategy::Both,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "track-2".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "reep".to_string(),
        };

        let commands = handler.on_event(&event).await?;
        assert!(matches!(
            commands.get(0),
            Some(Command::Tts {
                text,
                wait_input_timeout: Some(timeout),
                ..
            }) if text == "Final answer" && *timeout == 10000
        ));
        assert_eq!(rag.recorded_queries(), vec!["policy".to_string()]);

        Ok(())
    }

    #[tokio::test]
    async fn test_full_dialogue_flow() -> Result<()> {
        let responses = vec![
            "Hello! How can I help you today?".to_string(),
            r#"{"text": "I can help with that. Anything else?", "waitInputTimeout": 5000}"#
                .to_string(),
            r#"{"text": "Goodbye!", "tools": [{"name": "hangup", "reason": "completed"}]}"#
                .to_string(),
        ];

        let provider = Arc::new(TestProvider::new(responses));
        let config = LlmConfig {
            greeting: Some("Welcome to the voice assistant.".to_string()),
            ..Default::default()
        };

        let mut handler = LlmHandler::with_provider(
            config,
            provider,
            Arc::new(NoopRagRetriever),
            InterruptionStrategy::Both,
        );

        // 1. Start the dialogue
        let commands = handler.on_start().await?;
        assert_eq!(commands.len(), 1);
        if let Command::Tts { text, .. } = &commands[0] {
            assert_eq!(text, "Welcome to the voice assistant.");
        } else {
            panic!("Expected Tts command");
        }

        // 2. User says something
        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "I need help".to_string(),
        };
        let commands = handler.on_event(&event).await?;
        // "Hello! How can I help you today?" -> split into two
        assert_eq!(commands.len(), 2);
        if let Command::Tts { text, .. } = &commands[0] {
            assert!(text.contains("Hello"));
        } else {
            panic!("Expected Tts command");
        }

        // 3. User says something else
        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 1,
            start_time: None,
            end_time: None,
            text: "Tell me a joke".to_string(),
        };
        let commands = handler.on_event(&event).await?;
        assert_eq!(commands.len(), 1);
        if let Command::Tts {
            text,
            wait_input_timeout,
            ..
        } = &commands[0]
        {
            assert_eq!(text, "I can help with that. Anything else?");
            assert_eq!(*wait_input_timeout, Some(5000));
        } else {
            panic!("Expected Tts command");
        }

        // 4. User says goodbye
        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 2,
            start_time: None,
            end_time: None,
            text: "That's all, thanks".to_string(),
        };
        let commands = handler.on_event(&event).await?;
        // Should have Tts with auto_hangup
        assert_eq!(commands.len(), 1);

        let has_tts_hangup = commands.iter().any(|c| {
            matches!(
                c,
                Command::Tts {
                    text,
                    auto_hangup: Some(true),
                    ..
                } if text == "Goodbye!"
            )
        });

        assert!(has_tts_hangup);

        Ok(())
    }

    #[tokio::test]
    async fn test_xml_tools_and_sentence_splitting() -> Result<()> {
        let responses = vec!["Hello! <refer to=\"sip:123\"/> How are you? <hangup/>".to_string()];
        let provider = Arc::new(TestProvider::new(responses));
        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(NoopRagRetriever),
            InterruptionStrategy::Both,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hi".to_string(),
        };

        let commands = handler.on_event(&event).await?;

        // Expected commands:
        // 1. TTS "Hello! "
        // 2. Refer "sip:123"
        // 3. TTS " How are you? "
        // 4. Hangup
        assert_eq!(commands.len(), 4);

        if let Command::Tts {
            text,
            play_id: pid1,
            ..
        } = &commands[0]
        {
            assert!(text.contains("Hello"));
            assert!(pid1.is_some());

            if let Command::Refer { callee, .. } = &commands[1] {
                assert_eq!(callee, "sip:123");
            } else {
                panic!("Expected Refer");
            }

            if let Command::Tts {
                text,
                play_id: pid2,
                ..
            } = &commands[2]
            {
                assert!(text.contains("How are you"));
                assert_eq!(*pid1, *pid2); // Same play_id
            } else {
                panic!("Expected Tts");
            }

            if let Command::Hangup { .. } = &commands[3] {
                // Ok
            } else {
                panic!("Expected Hangup");
            }
        } else {
            panic!("Expected Tts");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_interruption_logic() -> Result<()> {
        let provider = Arc::new(TestProvider::new(vec!["Some long response".to_string()]));
        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(NoopRagRetriever),
            InterruptionStrategy::Both,
        );

        // 1. Trigger a response
        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hello".to_string(),
        };
        handler.on_event(&event).await?;
        assert!(handler.is_speaking);

        // Sleep to bypass the 800ms interruption guard
        tokio::time::sleep(std::time::Duration::from_millis(850)).await;

        // 2. Simulate user starting to speak (AsrDelta)
        let event = SessionEvent::AsrDelta {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "I...".to_string(),
        };
        let commands = handler.on_event(&event).await?;
        assert_eq!(commands.len(), 1);
        assert!(matches!(commands[0], Command::Interrupt { .. }));
        assert!(!handler.is_speaking);

        Ok(())
    }

    #[tokio::test]
    async fn test_rag_iteration_limit() -> Result<()> {
        // Provider that always returns a RAG tool call
        let rag_instruction = r#"{"tools": [{"name": "rag", "query": "endless"}]}"#;
        let provider = Arc::new(TestProvider::new(vec![
            rag_instruction.to_string(),
            rag_instruction.to_string(),
            rag_instruction.to_string(),
            rag_instruction.to_string(),
            "Should not reach here".to_string(),
        ]));

        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(RecordingRag::new()),
            InterruptionStrategy::Both,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "loop".to_string(),
        };

        let commands = handler.on_event(&event).await?;
        // After 3 attempts (MAX_RAG_ATTEMPTS), it should stop and return the last raw response
        assert_eq!(commands.len(), 1);
        if let Command::Tts { text, .. } = &commands[0] {
            assert_eq!(text, rag_instruction);
        }

        Ok(())
    }
}
