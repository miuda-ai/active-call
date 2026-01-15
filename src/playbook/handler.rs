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
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{info, warn};

static RE_HANGUP: Lazy<Regex> = Lazy::new(|| Regex::new(r"<hangup\s*/>").unwrap());
static RE_REFER: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<refer\s+to="([^"]+)"\s*/>"#).unwrap());
static RE_PLAY: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<play\s+file="([^"]+)"\s*/>"#).unwrap());
static RE_GOTO: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<goto\s+scene="([^"]+)"\s*/>"#).unwrap());
static RE_SENTENCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)[.!?。！？\n]\s*").unwrap());
static FILLERS: Lazy<std::collections::HashSet<String>> = Lazy::new(|| {
    let mut s = std::collections::HashSet::new();
    let default_fillers = ["嗯", "啊", "哦", "那个", "那个...", "uh", "um", "ah"];

    if let Ok(content) = std::fs::read_to_string("config/fillers.txt") {
        for line in content.lines() {
            let trimmed = line.trim().to_lowercase();
            if !trimmed.is_empty() {
                s.insert(trimmed);
            }
        }
    }

    if s.is_empty() {
        for f in default_fillers {
            s.insert(f.to_string());
        }
    }
    s
});

use super::ChatMessage;
use super::InterruptionStrategy;
use super::LlmConfig;
use super::dialogue::DialogueHandler;

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

pub struct RealtimeResponse {
    pub audio_delta: Option<Vec<u8>>,
    pub text_delta: Option<String>,
    pub function_call: Option<ToolInvocation>,
    pub speech_started: bool,
}

#[async_trait]
pub trait RealtimeProvider: Send + Sync {
    async fn connect(&self, config: &LlmConfig) -> Result<()>;
    async fn send_audio(&self, audio: &[i16]) -> Result<()>;
    async fn subscribe(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<RealtimeResponse>> + Send>>>;
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

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "name", rename_all = "lowercase")]
pub enum ToolInvocation {
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
    interruption_config: super::InterruptionConfig,
    dtmf_config: Option<HashMap<String, super::DtmfAction>>,
    history: Vec<ChatMessage>,
    provider: Arc<dyn LlmProvider>,
    rag_retriever: Arc<dyn RagRetriever>,
    is_speaking: bool,
    event_sender: Option<crate::event::EventSender>,
    last_asr_final_at: Option<std::time::Instant>,
    last_tts_start_at: Option<std::time::Instant>,
    call: Option<crate::call::ActiveCallRef>,
    scenes: HashMap<String, super::Scene>,
    current_scene_id: Option<String>,
}

impl LlmHandler {
    pub fn new(
        config: LlmConfig,
        interruption: super::InterruptionConfig,
        scenes: HashMap<String, super::Scene>,
        dtmf: Option<HashMap<String, super::DtmfAction>>,
        initial_scene_id: Option<String>,
    ) -> Self {
        Self::with_provider(
            config,
            Arc::new(DefaultLlmProvider::new()),
            Arc::new(NoopRagRetriever),
            interruption,
            scenes,
            dtmf,
            initial_scene_id,
        )
    }

    pub fn with_provider(
        config: LlmConfig,
        provider: Arc<dyn LlmProvider>,
        rag_retriever: Arc<dyn RagRetriever>,
        interruption: super::InterruptionConfig,
        scenes: HashMap<String, super::Scene>,
        dtmf: Option<HashMap<String, super::DtmfAction>>,
        initial_scene_id: Option<String>,
    ) -> Self {
        let mut history = Vec::new();
        let prompt = config.prompt.clone().unwrap_or_default();
        let system_prompt = Self::build_system_prompt(&prompt);

        history.push(ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        });

        Self {
            config,
            interruption_config: interruption,
            dtmf_config: dtmf,
            history,
            provider,
            rag_retriever,
            is_speaking: false,
            event_sender: None,
            last_asr_final_at: None,
            last_tts_start_at: None,
            call: None,
            scenes,
            current_scene_id: initial_scene_id,
        }
    }

    fn build_system_prompt(prompt: &str) -> String {
        format!(
            "{}\n\n\
            Tool usage instructions:\n\
            - To hang up the call, use: <hangup/>\n\
            - To transfer the call, use: <refer to=\"sip:xxxx\"/>\n\
            - To play an audio file, use: <play file=\"path/to/file.wav\"/>\n\
            - To switch to another scene, use: <goto scene=\"scene_id\"/>\n\
            Use these XML-like tags instead of JSON. Efficiency is key. \
            Output your response in short sentences. Each sentence will be played as soon as it is finished.",
            prompt
        )
    }

    fn get_dtmf_action(&self, digit: &str) -> Option<super::DtmfAction> {
        // 1. Check current scene
        if let Some(scene_id) = &self.current_scene_id {
            if let Some(scene) = self.scenes.get(scene_id) {
                if let Some(dtmf) = &scene.dtmf {
                    if let Some(action) = dtmf.get(digit) {
                        return Some(action.clone());
                    }
                }
            }
        }

        // 2. Check global
        if let Some(dtmf) = &self.dtmf_config {
            if let Some(action) = dtmf.get(digit) {
                return Some(action.clone());
            }
        }

        None
    }

    async fn handle_dtmf_action(&mut self, action: super::DtmfAction) -> Result<Vec<Command>> {
        match action {
            super::DtmfAction::Goto { scene } => {
                info!("DTMF action: switch to scene {}", scene);
                self.switch_to_scene(&scene, true).await
            }
            super::DtmfAction::Transfer { target } => {
                info!("DTMF action: transfer to {}", target);
                Ok(vec![Command::Refer {
                    caller: String::new(),
                    callee: target,
                    options: None,
                }])
            }
            super::DtmfAction::Hangup => {
                info!("DTMF action: hangup");
                Ok(vec![Command::Hangup {
                    reason: Some("DTMF Hangup".to_string()),
                    initiator: Some("ai".to_string()),
                }])
            }
        }
    }

    async fn switch_to_scene(
        &mut self,
        scene_id: &str,
        trigger_response: bool,
    ) -> Result<Vec<Command>> {
        if let Some(scene) = self.scenes.get(scene_id).cloned() {
            info!("Switching to scene: {}", scene_id);
            self.current_scene_id = Some(scene_id.to_string());
            // Update system prompt in history
            let system_prompt = Self::build_system_prompt(&scene.prompt);
            if let Some(first_msg) = self.history.get_mut(0) {
                if first_msg.role == "system" {
                    first_msg.content = system_prompt;
                }
            }

            let mut commands = Vec::new();
            if let Some(url) = &scene.play {
                commands.push(Command::Play {
                    url: url.clone(),
                    play_id: None,
                    auto_hangup: None,
                    wait_input_timeout: None,
                });
            }

            if trigger_response {
                let response_cmds = self.generate_response().await?;
                commands.extend(response_cmds);
            }
            Ok(commands)
        } else {
            warn!("Scene not found: {}", scene_id);
            Ok(vec![])
        }
    }

    pub fn get_history_ref(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn get_current_scene_id(&self) -> Option<String> {
        self.current_scene_id.clone()
    }

    pub fn set_call(&mut self, call: crate::call::ActiveCallRef) {
        self.call = Some(call);
    }

    pub fn set_event_sender(&mut self, sender: crate::event::EventSender) {
        self.event_sender = Some(sender.clone());
        // Send initial greeting if any
        if let Some(greeting) = &self.config.greeting {
            let _ = sender.send(crate::event::SessionEvent::AddHistory {
                sender: Some("system".to_string()),
                timestamp: crate::media::get_timestamp(),
                speaker: "assistant".to_string(),
                text: greeting.clone(),
            });
        }
    }

    fn send_debug_event(&self, key: &str, data: serde_json::Value) {
        if let Some(sender) = &self.event_sender {
            let timestamp = crate::media::get_timestamp();
            // If this is an LLM response, sync it with history
            if key == "llm_response" {
                if let Some(text) = data.get("response").and_then(|v| v.as_str()) {
                    let _ = sender.send(crate::event::SessionEvent::AddHistory {
                        sender: Some("llm".to_string()),
                        timestamp,
                        speaker: "assistant".to_string(),
                        text: text.to_string(),
                    });
                }
            }

            let event = crate::event::SessionEvent::Metrics {
                timestamp,
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
        let play_id = uuid::Uuid::new_v4().to_string();

        if let Some(sender) = &self.event_sender {
            let _ = sender.send(crate::event::SessionEvent::Metrics {
                timestamp: crate::media::get_timestamp(),
                key: "tts_play_id_map".to_string(),
                duration: 0,
                data: serde_json::json!({
                    "playId": play_id,
                    "text": text,
                }),
            });
        }

        Command::Tts {
            text,
            speaker: None,
            play_id: Some(play_id),
            auto_hangup,
            streaming: None,
            end_of_stream: None,
            option: None,
            wait_input_timeout: Some(timeout),
            base64: None,
        }
    }

    async fn generate_response(&mut self) -> Result<Vec<Command>> {
        let start_time = crate::media::get_timestamp();
        let play_id = uuid::Uuid::new_v4().to_string();

        // Send debug event - LLM call started
        self.send_debug_event(
            "llm_call_start",
            json!({
                "history_length": self.history.len(),
                "playId": play_id,
            }),
        );

        let mut stream = self
            .provider
            .call_stream(&self.config, &self.history)
            .await?;

        let mut full_content = String::new();
        let mut buffer = String::new();
        let mut commands = Vec::new();
        let mut is_json_mode = false;
        let mut checked_json_mode = false;
        let mut first_token_time = None;

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    warn!("LLM stream error: {}", e);
                    break;
                }
            };

            if first_token_time.is_none() && !chunk.trim().is_empty() {
                first_token_time = Some(crate::media::get_timestamp());
            }

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
        let end_time = crate::media::get_timestamp();
        self.send_debug_event(
            "llm_response",
            json!({
                "response": full_content,
                "is_json_mode": is_json_mode,
                "duration": end_time - start_time,
                "ttfb": first_token_time.map(|t| t - start_time).unwrap_or(0),
                "playId": play_id,
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
                self.last_tts_start_at = Some(std::time::Instant::now());
            }
            Ok(commands)
        }
    }

    fn extract_streaming_commands(
        &mut self,
        buffer: &mut String,
        play_id: &str,
        is_final: bool,
    ) -> Vec<Command> {
        let mut commands = Vec::new();

        loop {
            let hangup_pos = RE_HANGUP.find(buffer);
            let refer_pos = RE_REFER.captures(buffer);
            let play_pos = RE_PLAY.captures(buffer);
            let goto_pos = RE_GOTO.captures(buffer);
            let sentence_pos = RE_SENTENCE.find(buffer);

            // Find the first occurrence
            let mut positions = Vec::new();
            if let Some(m) = hangup_pos {
                positions.push((m.start(), 0));
            }
            if let Some(caps) = &refer_pos {
                positions.push((caps.get(0).unwrap().start(), 1));
            }
            if let Some(caps) = &play_pos {
                positions.push((caps.get(0).unwrap().start(), 3));
            }
            if let Some(caps) = &goto_pos {
                positions.push((caps.get(0).unwrap().start(), 4));
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
                    3 => {
                        // Play audio
                        let caps = RE_PLAY.captures(buffer).unwrap();
                        let mat = caps.get(0).unwrap();
                        let url = caps.get(1).unwrap().as_str().to_string();

                        let prefix = buffer[..pos].to_string();
                        if !prefix.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                prefix,
                                play_id.to_string(),
                                None,
                            ));
                        }
                        commands.push(Command::Play {
                            url,
                            play_id: None,
                            auto_hangup: None,
                            wait_input_timeout: None,
                        });
                        buffer.drain(..mat.end());
                    }
                    4 => {
                        // Goto Scene
                        let caps = RE_GOTO.captures(buffer).unwrap();
                        let mat = caps.get(0).unwrap();
                        let scene_id = caps.get(1).unwrap().as_str().to_string();

                        let prefix = buffer[..pos].to_string();
                        if !prefix.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                prefix,
                                play_id.to_string(),
                                None,
                            ));
                        }

                        info!("Switching to scene (from stream): {}", scene_id);
                        if let Some(scene) = self.scenes.get(&scene_id) {
                            self.current_scene_id = Some(scene_id);
                            // Update system prompt in history
                            let system_prompt = Self::build_system_prompt(&scene.prompt);
                            if let Some(first_msg) = self.history.get_mut(0) {
                                if first_msg.role == "system" {
                                    first_msg.content = system_prompt;
                                }
                            }
                        } else {
                            warn!("Scene not found: {}", scene_id);
                        }

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
                self.last_tts_start_at = Some(std::time::Instant::now());
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

fn is_likely_filler(text: &str) -> bool {
    let trimmed = text.trim().to_lowercase();
    FILLERS.contains(&trimmed)
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
        self.last_tts_start_at = Some(std::time::Instant::now());

        let mut commands = Vec::new();

        // Check if current scene has an audio file to play
        if let Some(scene_id) = &self.current_scene_id {
            if let Some(scene) = self.scenes.get(scene_id) {
                if let Some(audio_file) = &scene.play {
                    commands.push(Command::Play {
                        url: audio_file.clone(),
                        play_id: None,
                        auto_hangup: None,
                        wait_input_timeout: None,
                    });
                }
            }
        }

        if let Some(greeting) = &self.config.greeting {
            self.is_speaking = true;
            commands.push(self.create_tts_command(greeting.clone(), None, None));
            return Ok(commands);
        }

        let response_commands = self.generate_response().await?;
        commands.extend(response_commands);
        Ok(commands)
    }

    async fn on_event(&mut self, event: &SessionEvent) -> Result<Vec<Command>> {
        match event {
            SessionEvent::Dtmf { digit, .. } => {
                info!("DTMF received: {}", digit);
                let action = self.get_dtmf_action(digit);
                if let Some(action) = action {
                    return self.handle_dtmf_action(action).await;
                }
                Ok(vec![])
            }

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

            SessionEvent::AsrDelta { is_filler, .. } | SessionEvent::Speaking { is_filler, .. } => {
                let strategy = self.interruption_config.strategy;
                let should_check = match (strategy, event) {
                    (InterruptionStrategy::None, _) => false,
                    (InterruptionStrategy::Vad, SessionEvent::Speaking { .. }) => true,
                    (InterruptionStrategy::Asr, SessionEvent::AsrDelta { .. }) => true,
                    (InterruptionStrategy::Both, _) => true,
                    _ => false,
                };

                if self.is_speaking && should_check {
                    // 1. Protection Period Check
                    if let Some(last_start) = self.last_tts_start_at {
                        let ignore_ms = self.interruption_config.ignore_first_ms.unwrap_or(800);
                        if last_start.elapsed().as_millis() < ignore_ms as u128 {
                            return Ok(vec![]);
                        }
                    }

                    // 2. Filler Word Check
                    if self.interruption_config.filler_word_filter.unwrap_or(false) {
                        if let Some(true) = is_filler {
                            return Ok(vec![]);
                        }
                        // Secondary text-based filler check for ASR Delta
                        if let SessionEvent::AsrDelta { text, .. } = event {
                            if is_likely_filler(text) {
                                return Ok(vec![]);
                            }
                        }
                    }

                    // 3. Stale event check (already had one in original code)
                    if let Some(last_final) = self.last_asr_final_at {
                        if last_final.elapsed().as_millis() < 500 {
                            return Ok(vec![]);
                        }
                    }

                    info!("Smart interruption detected, stopping playback");
                    self.is_speaking = false;
                    return Ok(vec![Command::Interrupt {
                        graceful: Some(true),
                        fade_out_ms: self.interruption_config.volume_fade_ms,
                    }]);
                }
                Ok(vec![])
            }

            SessionEvent::Eou { completed, .. } => {
                if *completed && self.is_speaking == false {
                    info!("EOU detected, triggering early response");
                    return self.generate_response().await;
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

            SessionEvent::FunctionCall {
                name, arguments, ..
            } => {
                info!(
                    "Function call from Realtime: {} with args {}",
                    name, arguments
                );
                let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
                match name.as_str() {
                    "hangup_call" => Ok(vec![Command::Hangup {
                        reason: args["reason"].as_str().map(|s| s.to_string()),
                        initiator: Some("ai".to_string()),
                    }]),
                    "transfer_call" | "refer_call" => {
                        if let Some(callee) = args["callee"]
                            .as_str()
                            .or_else(|| args["callee_uri"].as_str())
                        {
                            Ok(vec![Command::Refer {
                                caller: String::new(),
                                callee: callee.to_string(),
                                options: None,
                            }])
                        } else {
                            warn!("No callee provided for transfer_call");
                            Ok(vec![])
                        }
                    }
                    "goto_scene" => {
                        if let Some(scene) = args["scene"].as_str() {
                            self.switch_to_scene(scene, false).await
                        } else {
                            Ok(vec![])
                        }
                    }
                    _ => {
                        warn!("Unhandled function call: {}", name);
                        Ok(vec![])
                    }
                }
            }

            _ => Ok(vec![]),
        }
    }

    async fn get_history(&self) -> Vec<ChatMessage> {
        self.history.clone()
    }

    async fn summarize(&mut self, prompt: &str) -> Result<String> {
        info!("Generating summary with prompt: {}", prompt);
        let mut summary_history = self.history.clone();
        summary_history.push(ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        });

        self.provider.call(&self.config, &summary_history).await
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
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "track-1".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hello".to_string(),
            is_filler: None,
            confidence: None,
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
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "track-2".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "reep".to_string(),
            is_filler: None,
            confidence: None,
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
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
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
            is_filler: None,
            confidence: None,
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
            is_filler: None,
            confidence: None,
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
            is_filler: None,
            confidence: None,
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
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hi".to_string(),
            is_filler: None,
            confidence: None,
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
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
        );

        // 1. Trigger a response
        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hello".to_string(),
            is_filler: None,
            confidence: None,
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
            is_filler: None,
            confidence: None,
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
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
        );

        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "loop".to_string(),
            is_filler: None,
            confidence: None,
        };

        let commands = handler.on_event(&event).await?;
        // After 3 attempts (MAX_RAG_ATTEMPTS), it should stop and return the last raw response
        assert_eq!(commands.len(), 1);
        if let Command::Tts { text, .. } = &commands[0] {
            assert_eq!(text, rag_instruction);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_interruption_protection_period() -> Result<()> {
        let provider = Arc::new(TestProvider::new(vec!["Some long response".to_string()]));
        let mut config = crate::playbook::InterruptionConfig::default();
        config.ignore_first_ms = Some(800);

        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(NoopRagRetriever),
            config,
            HashMap::new(),
            None,
            None,
        );

        // 1. Trigger a response
        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hello".to_string(),
            is_filler: None,
            confidence: None,
        };
        handler.on_event(&event).await?;
        assert!(handler.is_speaking);

        // 2. Simulate user starting to speak immediately (AsrDelta)
        let event = SessionEvent::AsrDelta {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "I...".to_string(),
            is_filler: None,
            confidence: None,
        };
        let commands = handler.on_event(&event).await?;
        // Should be ignored due to protection period
        assert_eq!(commands.len(), 0);
        assert!(handler.is_speaking);

        Ok(())
    }

    #[tokio::test]
    async fn test_interruption_filler_word() -> Result<()> {
        let provider = Arc::new(TestProvider::new(vec!["Some long response".to_string()]));
        let mut config = crate::playbook::InterruptionConfig::default();
        config.filler_word_filter = Some(true);
        config.ignore_first_ms = Some(0); // Disable protection period for this test

        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(NoopRagRetriever),
            config,
            HashMap::new(),
            None,
            None,
        );

        // 1. Trigger a response
        let event = SessionEvent::AsrFinal {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "hello".to_string(),
            is_filler: None,
            confidence: None,
        };
        handler.on_event(&event).await?;
        assert!(handler.is_speaking);

        // Sleep to bypass the 500ms stale event guard
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // 2. Simulate user saying "uh" (filler)
        let event = SessionEvent::AsrDelta {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "uh".to_string(),
            is_filler: Some(true),
            confidence: None,
        };
        let commands = handler.on_event(&event).await?;
        // Should be ignored
        assert_eq!(commands.len(), 0);
        assert!(handler.is_speaking);

        // 3. Simulate user saying "Wait" (not filler)
        let event = SessionEvent::AsrDelta {
            track_id: "test".to_string(),
            timestamp: 0,
            index: 0,
            start_time: None,
            end_time: None,
            text: "Wait".to_string(),
            is_filler: Some(false),
            confidence: None,
        };
        let commands = handler.on_event(&event).await?;
        // Should trigger interruption
        assert_eq!(commands.len(), 1);
        assert!(matches!(commands[0], Command::Interrupt { .. }));

        Ok(())
    }

    #[tokio::test]
    async fn test_eou_early_response() -> Result<()> {
        let provider = Arc::new(TestProvider::new(vec![
            "End of Utterance response".to_string(),
        ]));
        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(NoopRagRetriever),
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
        );

        // 1. Receive EOU
        let event = SessionEvent::Eou {
            track_id: "test".to_string(),
            timestamp: 0,
            completed: true,
        };
        let commands = handler.on_event(&event).await?;
        assert_eq!(commands.len(), 1);
        if let Command::Tts { text, .. } = &commands[0] {
            assert_eq!(text, "End of Utterance response");
        } else {
            panic!("Expected Tts");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_summary_and_history() -> Result<()> {
        let provider = Arc::new(TestProvider::new(vec!["Test summary".to_string()]));
        let mut handler = LlmHandler::with_provider(
            LlmConfig::default(),
            provider,
            Arc::new(NoopRagRetriever),
            crate::playbook::InterruptionConfig::default(),
            HashMap::new(),
            None,
            None,
        );

        // Add some history
        handler.history.push(ChatMessage {
            role: "user".to_string(),
            content: "Hello".to_string(),
        });
        handler.history.push(ChatMessage {
            role: "assistant".to_string(),
            content: "Hi there".to_string(),
        });

        // Test get_history
        let history = handler.get_history().await;
        assert_eq!(history.len(), 3); // system + user + assistant

        // Test summarize
        let summary = handler.summarize("Summarize this").await?;
        assert_eq!(summary, "Test summary");

        Ok(())
    }
}
