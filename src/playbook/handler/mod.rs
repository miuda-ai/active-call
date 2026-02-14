use crate::call::Command;
use crate::event::SessionEvent;
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dtmf_collector_tests;

static RE_HANGUP: Lazy<Regex> = Lazy::new(|| Regex::new(r"<hangup\s*/>").unwrap());
static RE_REFER: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<refer\s+to="([^"]+)"\s*/>"#).unwrap());
static RE_PLAY: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<play\s+file="([^"]+)"\s*/>"#).unwrap());
static RE_GOTO: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<goto\s+scene="([^"]+)"\s*/>"#).unwrap());
static RE_SET_VAR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"<set_var\s+key="([^"]+)"\s+value=["'](.+?)["']\s*/>"#).unwrap());
static RE_HTTP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"<http\s+url="([^"]+)"(?:\s+method="([^"]+)")?(?:\s+body="([^"]+)")?\s*/>"#)
        .unwrap()
});
static RE_COLLECT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"<collect\s+type="([^"]+)"\s+var="([^"]+)"(?:\s+prompt="([^"]*)")?\s*/>"#).unwrap()
});
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

pub mod provider;
pub mod rag;
pub mod types;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Hangup,
    Refer,
    Sentence,
    Play,
    Goto,
    SetVar,
    Http,
    Collect,
}

pub use provider::*;
pub use rag::*;
pub use types::*;

const MAX_RAG_ATTEMPTS: usize = 3;

/// Runtime state for an active DTMF digit collection session
#[derive(Debug, Clone)]
pub struct CollectorState {
    /// Name of the collector type being used (key into dtmf_collectors)
    pub collector_type: String,
    /// Variable name to store the collected digits
    pub var_name: String,
    /// Resolved config from the collector template
    pub config: super::DtmfCollectorConfig,
    /// Buffer of collected digits so far
    pub buffer: String,
    /// When collection started
    pub start_time: std::time::Instant,
    /// When the last digit was received
    pub last_digit_time: std::time::Instant,
    /// Number of retries attempted
    pub retry_count: u32,
}

pub struct LlmHandler {
    config: LlmConfig,
    interruption_config: super::InterruptionConfig,
    global_follow_up_config: Option<super::FollowUpConfig>,
    dtmf_config: Option<HashMap<String, super::DtmfAction>>,
    dtmf_collectors: Option<HashMap<String, super::DtmfCollectorConfig>>,
    history: Vec<ChatMessage>,
    provider: Arc<dyn LlmProvider>,
    rag_retriever: Arc<dyn RagRetriever>,
    is_speaking: bool,
    is_hanging_up: bool,
    consecutive_follow_ups: u32,
    last_interaction_at: std::time::Instant,
    event_sender: Option<crate::event::EventSender>,
    last_asr_final_at: Option<std::time::Instant>,
    last_tts_start_at: Option<std::time::Instant>,
    last_robot_msg_at: Option<std::time::Instant>,
    call: Option<crate::call::ActiveCallRef>,
    scenes: HashMap<String, super::Scene>,
    current_scene_id: Option<String>,
    client: Client,
    sip_config: Option<crate::SipOption>,
    /// Active DTMF digit collector state (None when not collecting)
    collector_state: Option<CollectorState>,
}

impl LlmHandler {
    pub fn new(
        config: LlmConfig,
        interruption: super::InterruptionConfig,
        global_follow_up_config: Option<super::FollowUpConfig>,
        scenes: HashMap<String, super::Scene>,
        dtmf: Option<HashMap<String, super::DtmfAction>>,
        dtmf_collectors: Option<HashMap<String, super::DtmfCollectorConfig>>,
        initial_scene_id: Option<String>,
        sip_config: Option<crate::SipOption>,
    ) -> Self {
        Self::with_provider(
            config,
            Arc::new(DefaultLlmProvider::new()),
            Arc::new(NoopRagRetriever),
            interruption,
            global_follow_up_config,
            scenes,
            dtmf,
            dtmf_collectors,
            initial_scene_id,
            sip_config,
        )
    }

    pub fn with_provider(
        config: LlmConfig,
        provider: Arc<dyn LlmProvider>,
        rag_retriever: Arc<dyn RagRetriever>,
        interruption: super::InterruptionConfig,
        global_follow_up_config: Option<super::FollowUpConfig>,
        scenes: HashMap<String, super::Scene>,
        dtmf: Option<HashMap<String, super::DtmfAction>>,
        dtmf_collectors: Option<HashMap<String, super::DtmfCollectorConfig>>,
        initial_scene_id: Option<String>,
        sip_config: Option<crate::SipOption>,
    ) -> Self {
        let mut history = Vec::new();
        let system_prompt = Self::build_system_prompt(&config, None, dtmf_collectors.as_ref());

        history.push(ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        });

        Self {
            config,
            interruption_config: interruption,
            global_follow_up_config,
            dtmf_config: dtmf,
            dtmf_collectors,
            history,
            provider,
            rag_retriever,
            is_speaking: false,
            is_hanging_up: false,
            consecutive_follow_ups: 0,
            last_interaction_at: std::time::Instant::now(),
            event_sender: None,
            last_asr_final_at: None,
            last_tts_start_at: None,
            last_robot_msg_at: None,
            call: None,
            scenes,
            current_scene_id: initial_scene_id,
            client: Client::new(),
            sip_config,
            collector_state: None,
        }
    }

    fn build_system_prompt(
        config: &LlmConfig,
        scene_prompt: Option<&str>,
        dtmf_collectors: Option<&HashMap<String, super::DtmfCollectorConfig>>,
    ) -> String {
        let base_prompt =
            scene_prompt.unwrap_or_else(|| config.prompt.as_deref().unwrap_or_default());
        let mut features_prompt = String::new();

        if let Some(features) = &config.features {
            let lang = config.language.as_deref().unwrap_or("zh");
            for feature in features {
                match Self::load_feature_snippet(feature, lang) {
                    Ok(snippet) => {
                        features_prompt.push_str(&format!("\n- {}", snippet));
                    }
                    Err(e) => {
                        warn!("Failed to load feature snippet {}: {}", feature, e);
                    }
                }
            }
        }

        let features_section = if features_prompt.is_empty() {
            String::new()
        } else {
            format!("\n\n### Enhanced Capabilities:{}\n", features_prompt)
        };

        // Load tool instructions - either custom or language-specific default
        let tool_instructions = if let Some(custom) = &config.tool_instructions {
            custom.clone()
        } else {
            let lang = config.language.as_deref().unwrap_or("zh");
            Self::load_feature_snippet("tool_instructions", lang)
                .unwrap_or_else(|_| {
                    // Fallback to English if loading fails
                    Self::load_feature_snippet("tool_instructions", "en")
                        .unwrap_or_else(|_| {
                            // Ultimate fallback to hardcoded English
                            "Tool usage instructions:\n\
                            - To hang up the call, output: <hangup/>\n\
                            - To transfer the call, output: <refer to=\"sip:xxxx\"/>\n\
                            - To play an audio file, output: <play file=\"path/to/file.wav\"/>\n\
                            - To switch to another scene, output: <goto scene=\"scene_id\"/>\n\
                            - To call an external HTTP API, output JSON:\n\
                              ```json\n\
                              {{ \"tools\": [{{ \"name\": \"http\", \"url\": \"...\", \"method\": \"POST\", \"body\": {{ ... }} }}] }}\n\
                              ```\n\
                            Please use XML tags for simple actions and JSON blocks for tool calls. \
                            Output your response in short sentences. Each sentence will be played as soon as it is finished."
                                .to_string()
                        })
                })
        };

        let collector_section = Self::generate_collector_instructions(dtmf_collectors);

        format!(
            "{}{}\n\n{}\n{}",
            base_prompt, features_section, tool_instructions, collector_section
        )
    }

    fn load_feature_snippet(feature: &str, lang: &str) -> Result<String> {
        let path = format!("features/{}.{}.md", feature, lang);
        let content = std::fs::read_to_string(path)?;
        Ok(content.trim().to_string())
    }

    /// Generate LLM prompt instructions for available DTMF digit collectors
    fn generate_collector_instructions(
        collectors: Option<&HashMap<String, super::DtmfCollectorConfig>>,
    ) -> String {
        let collectors = match collectors {
            Some(c) if !c.is_empty() => c,
            _ => return String::new(),
        };

        let mut doc = String::from("\n### DTMF Digit Collection\n\n");
        doc.push_str(
            "When you need to collect numeric input from the user (such as phone numbers, \
             verification codes, ID numbers, etc.), use the DTMF digit collection command. \
             This is more accurate than voice recognition for numeric input.\n\n",
        );
        doc.push_str("**Usage:** Output the following XML tag to start collecting:\n");
        doc.push_str(
            "```\n<collect type=\"TYPE\" var=\"VAR_NAME\" prompt=\"PROMPT_TEXT\" />\n```\n\n",
        );
        doc.push_str("- `type`: The collector type (see available types below)\n");
        doc.push_str("- `var`: Variable name to store the collected digits\n");
        doc.push_str("- `prompt`: The voice prompt to play before collecting (tell the user what to input)\n\n");
        doc.push_str("**Available collector types:**\n\n");

        // Sort by key for deterministic output
        let mut sorted: Vec<_> = collectors.iter().collect();
        sorted.sort_by_key(|(k, _)| (*k).clone());

        for (name, config) in &sorted {
            let desc = config.description.as_deref().unwrap_or("No description");
            let mut details = Vec::new();
            if let Some(d) = config.digits {
                details.push(format!("{} digits", d));
            } else {
                if let Some(min) = config.min_digits {
                    details.push(format!("min {} digits", min));
                }
                if let Some(max) = config.max_digits {
                    details.push(format!("max {} digits", max));
                }
            }
            if let Some(fk) = &config.finish_key {
                details.push(format!("press {} to finish", fk));
            }
            let detail_str = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join(", "))
            };
            doc.push_str(&format!("- `{}`: {}{}\n", name, desc, detail_str));
        }

        doc.push_str("\n**Flow:**\n");
        doc.push_str("1. You output `<collect .../>` with a voice prompt\n");
        doc.push_str("2. The system plays your prompt, then enters digit collection mode (voice input is ignored)\n");
        doc.push_str("3. When collection completes, the system notifies you with the result\n");
        doc.push_str("4. You can access the collected value via `{{ var_name }}` in subsequent responses\n\n");
        doc.push_str(
            "**Important:** During collection the user can only input digits, not speak. ",
        );
        doc.push_str("If validation fails, the system will automatically retry. ");
        doc.push_str("After collection success or failure, continue the conversation naturally.\n");

        doc
    }

    /// Check if the collector has timed out and handle accordingly.
    /// Returns commands to execute (e.g., retry prompt or failure notification).
    pub async fn check_collector_timeout(&mut self) -> Result<Vec<Command>> {
        let state = match &self.collector_state {
            Some(s) => s,
            None => return Ok(vec![]),
        };

        let timeout_secs = state.config.timeout.unwrap_or(15) as u64;
        let inter_digit_timeout_secs = state.config.inter_digit_timeout.unwrap_or(5) as u64;

        // Check overall timeout
        if state.start_time.elapsed().as_secs() >= timeout_secs {
            info!(
                "DTMF collector overall timeout ({}s) for var={}",
                timeout_secs, state.var_name
            );
            let var_name = state.var_name.clone();
            let buffer = state.buffer.clone();
            let collector_type = state.collector_type.clone();
            let config = state.config.clone();
            let retry_count = state.retry_count;
            self.collector_state = None;

            if !buffer.is_empty() {
                // Try to validate what we have
                return self
                    .do_finish_collection(buffer, var_name, collector_type, config, retry_count)
                    .await;
            }

            // Nothing collected - notify LLM
            self.history.push(ChatMessage {
                role: "system".to_string(),
                content: format!(
                    "[DTMF collection timed out for '{}'. No digits were entered. Please guide the user.]",
                    var_name
                ),
            });
            return self.generate_response().await;
        }

        // Check inter-digit timeout (only if we have some digits)
        if !state.buffer.is_empty()
            && state.last_digit_time.elapsed().as_secs() >= inter_digit_timeout_secs
        {
            info!(
                "DTMF collector inter-digit timeout ({}s) for var={}, buffer={}",
                inter_digit_timeout_secs, state.var_name, state.buffer
            );
            let buffer = state.buffer.clone();
            let var_name = state.var_name.clone();
            let collector_type = state.collector_type.clone();
            let config = state.config.clone();
            let retry_count = state.retry_count;
            self.collector_state = None;
            return self
                .do_finish_collection(buffer, var_name, collector_type, config, retry_count)
                .await;
        }

        Ok(vec![])
    }

    /// Handle a DTMF digit while in collector mode
    async fn handle_collector_digit(&mut self, digit: &str) -> Result<Vec<Command>> {
        let state = self.collector_state.as_mut().unwrap();

        // Check if it's the finish key
        if let Some(ref finish_key) = state.config.finish_key.clone() {
            if digit == finish_key {
                info!("DTMF collector: finish key '{}' received", digit);
                let buffer = state.buffer.clone();
                let var_name = state.var_name.clone();
                let collector_type = state.collector_type.clone();
                let config = state.config.clone();
                let retry_count = state.retry_count;
                self.collector_state = None;
                return self
                    .do_finish_collection(buffer, var_name, collector_type, config, retry_count)
                    .await;
            }
        }

        // Append digit to buffer
        state.buffer.push_str(digit);
        state.last_digit_time = std::time::Instant::now();

        info!(
            "DTMF collector: digit '{}', buffer now '{}'",
            digit, state.buffer
        );

        // Check if we've reached the required digit count
        let effective_max = state.config.digits.or(state.config.max_digits);

        if let Some(max) = effective_max {
            if state.buffer.len() >= max as usize {
                // If no finish_key is configured, auto-complete at max digits
                if state.config.finish_key.is_none() {
                    info!("DTMF collector: reached max digits ({})", max);
                    let buffer = state.buffer.clone();
                    let var_name = state.var_name.clone();
                    let collector_type = state.collector_type.clone();
                    let config = state.config.clone();
                    let retry_count = state.retry_count;
                    self.collector_state = None;
                    return self
                        .do_finish_collection(buffer, var_name, collector_type, config, retry_count)
                        .await;
                }
            }
        }

        Ok(vec![])
    }

    /// Internal: finish collection with full state available
    async fn do_finish_collection(
        &mut self,
        buffer: String,
        var_name: String,
        collector_type: String,
        config: super::DtmfCollectorConfig,
        retry_count: u32,
    ) -> Result<Vec<Command>> {
        // Validate min digits
        let min = config.digits.or(config.min_digits).unwrap_or(0);
        if min > 0 && (buffer.len() as u32) < min {
            return self
                .retry_or_fail(
                    collector_type,
                    config,
                    retry_count,
                    var_name,
                    &format!("Expected at least {} digits, got {}", min, buffer.len()),
                )
                .await;
        }

        // Validate pattern
        if let Some(validation) = &config.validation {
            if let Ok(re) = regex::Regex::new(&validation.pattern) {
                if !re.is_match(&buffer) {
                    let msg = validation
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Input format is incorrect".to_string());
                    return self
                        .retry_or_fail(collector_type, config, retry_count, var_name, &msg)
                        .await;
                }
            }
        }

        // Validation passed - store the variable
        info!(
            "DTMF collector: successfully collected '{}' for var '{}'",
            buffer, var_name
        );

        if let Some(call) = &self.call {
            let mut state = call.call_state.write().await;
            let mut extras = state.extras.take().unwrap_or_default();
            extras.insert(var_name.clone(), serde_json::Value::String(buffer.clone()));
            state.extras = Some(extras);
        }

        // Notify LLM of the result
        self.history.push(ChatMessage {
            role: "system".to_string(),
            content: format!("[DTMF collection completed for '{}': {}]", var_name, buffer),
        });

        // Let LLM continue
        self.generate_response().await
    }

    /// Retry collection or fail after max retries
    async fn retry_or_fail(
        &mut self,
        collector_type: String,
        config: super::DtmfCollectorConfig,
        retry_count: u32,
        var_name: String,
        reason: &str,
    ) -> Result<Vec<Command>> {
        let max_retries = config.retry_times.unwrap_or(3);

        if retry_count >= max_retries {
            info!(
                "DTMF collector: max retries ({}) reached for var '{}'",
                max_retries, var_name
            );
            self.history.push(ChatMessage {
                role: "system".to_string(),
                content: format!(
                    "[DTMF collection failed for '{}' after {} retries: {}. Please guide the user to try again or use an alternative method.]",
                    var_name, max_retries, reason
                ),
            });
            return self.generate_response().await;
        }

        info!(
            "DTMF collector: retry {}/{} for var '{}': {}",
            retry_count + 1,
            max_retries,
            var_name,
            reason
        );

        // Restart collection with incremented retry count
        let now = std::time::Instant::now();
        self.collector_state = Some(CollectorState {
            collector_type,
            var_name,
            config: config.clone(),
            buffer: String::new(),
            start_time: now,
            last_digit_time: now,
            retry_count: retry_count + 1,
        });

        // Play error message
        let error_msg = config
            .validation
            .as_ref()
            .and_then(|v| v.error_message.clone())
            .unwrap_or_else(|| reason.to_string());

        Ok(vec![self.create_tts_command(error_msg, None, None)])
    }

    /// Start a DTMF collector from an LLM-generated <collect> command
    fn start_collector(&mut self, collector_type: &str, var_name: &str) -> bool {
        let config = match &self.dtmf_collectors {
            Some(collectors) => match collectors.get(collector_type) {
                Some(c) => c.clone(),
                None => {
                    warn!("Unknown DTMF collector type: {}", collector_type);
                    return false;
                }
            },
            None => {
                warn!("No DTMF collectors configured");
                return false;
            }
        };

        let now = std::time::Instant::now();
        self.collector_state = Some(CollectorState {
            collector_type: collector_type.to_string(),
            var_name: var_name.to_string(),
            config,
            buffer: String::new(),
            start_time: now,
            last_digit_time: now,
            retry_count: 0,
        });

        info!(
            "DTMF collector started: type={}, var={}",
            collector_type, var_name
        );
        true
    }

    /// Returns true if currently in DTMF digit collection mode
    pub fn is_collecting(&self) -> bool {
        self.collector_state.is_some()
    }

    fn get_dtmf_action(&self, digit: &str) -> Option<super::DtmfAction> {
        if let Some(scene_id) = &self.current_scene_id {
            if let Some(scene) = self.scenes.get(scene_id) {
                if let Some(dtmf) = &scene.dtmf {
                    if let Some(action) = dtmf.get(digit) {
                        return Some(action.clone());
                    }
                }
            }
        }

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
                let headers = self.render_sip_headers().await;
                Ok(vec![Command::Hangup {
                    reason: Some("DTMF Hangup".to_string()),
                    initiator: Some("ai".to_string()),
                    headers,
                }])
            }
        }
    }

    /// Get current extras (variables) from call_state for dynamic template rendering.
    async fn get_current_extras(&self) -> HashMap<String, serde_json::Value> {
        if let Some(call) = &self.call {
            let state = call.call_state.read().await;
            state.extras.clone().unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    /// Render a scene's prompt template using the latest variables from call_state.
    /// This enables `set_var` values to be reflected in system prompts dynamically.
    async fn render_scene_prompt(&self, scene: &super::Scene) -> String {
        let extras = self.get_current_extras().await;
        super::render_scene_prompt(scene, &extras)
    }

    async fn switch_to_scene(
        &mut self,
        scene_id: &str,
        trigger_response: bool,
    ) -> Result<Vec<Command>> {
        if let Some(scene) = self.scenes.get(scene_id).cloned() {
            info!("Switching to scene: {}", scene_id);
            self.current_scene_id = Some(scene_id.to_string());
            // Dynamically render the scene prompt with the latest variables
            let rendered_prompt = self.render_scene_prompt(&scene).await;
            let system_prompt = Self::build_system_prompt(
                &self.config,
                Some(&rendered_prompt),
                self.dtmf_collectors.as_ref(),
            );
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
            end_of_stream: Some(true),
            option: None,
            wait_input_timeout: Some(timeout),
            base64: None,
            cache_key: None,
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
        let mut full_reasoning = String::new();
        let mut buffer = String::new();
        let mut commands = Vec::new();
        let mut is_json_mode = false;
        let mut checked_json_mode = false;
        let mut first_token_time = None;

        while let Some(chunk_result) = stream.next().await {
            let event = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    warn!("LLM stream error: {}", e);
                    break;
                }
            };

            match event {
                LlmStreamEvent::Reasoning(text) => {
                    full_reasoning.push_str(&text);
                }
                LlmStreamEvent::Content(chunk) => {
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
                        let extracted = self
                            .extract_streaming_commands(&mut buffer, &play_id, false)
                            .await;
                        for cmd in extracted {
                            if let Some(call) = &self.call {
                                let _ = call.enqueue_command(cmd).await;
                            } else {
                                commands.push(cmd);
                            }
                        }
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
                "reasoning": full_reasoning,
                "is_json_mode": is_json_mode,
                "duration": end_time - start_time,
                "ttfb": first_token_time.map(|t| t - start_time).unwrap_or(0),
                "playId": play_id,
            }),
        );

        if is_json_mode {
            self.interpret_response(full_content).await
        } else {
            let extracted = self
                .extract_streaming_commands(&mut buffer, &play_id, true)
                .await;
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
                self.last_robot_msg_at = Some(std::time::Instant::now());
                self.is_speaking = true;
                self.last_tts_start_at = Some(std::time::Instant::now());
            }
            Ok(commands)
        }
    }

    async fn extract_streaming_commands(
        &mut self,
        buffer: &mut String,
        play_id: &str,
        is_final: bool,
    ) -> Vec<Command> {
        let mut commands = Vec::new();
        let mut pending_hangup: Option<(String, usize)> = None; // Store hangup prefix and position

        loop {
            let hangup_pos = RE_HANGUP.find(buffer);
            let refer_pos = RE_REFER.captures(buffer);
            let play_pos = RE_PLAY.captures(buffer);
            let goto_pos = RE_GOTO.captures(buffer);
            let set_var_pos = RE_SET_VAR.captures(buffer);
            let http_pos = RE_HTTP.captures(buffer);
            let collect_pos = RE_COLLECT.captures(buffer);
            let sentence_pos = RE_SENTENCE.find(buffer);

            // Find the first occurrence
            let mut positions: Vec<(usize, CommandKind)> = Vec::new();
            if let Some(m) = hangup_pos {
                positions.push((m.start(), CommandKind::Hangup));
            }
            if let Some(caps) = &refer_pos {
                positions.push((caps.get(0).unwrap().start(), CommandKind::Refer));
            }
            if let Some(caps) = &play_pos {
                positions.push((caps.get(0).unwrap().start(), CommandKind::Play));
            }
            if let Some(caps) = &goto_pos {
                positions.push((caps.get(0).unwrap().start(), CommandKind::Goto));
            }
            if let Some(caps) = &set_var_pos {
                positions.push((caps.get(0).unwrap().start(), CommandKind::SetVar));
            }
            if let Some(caps) = &http_pos {
                positions.push((caps.get(0).unwrap().start(), CommandKind::Http));
            }
            if let Some(caps) = &collect_pos {
                positions.push((caps.get(0).unwrap().start(), CommandKind::Collect));
            }
            if let Some(m) = sentence_pos {
                positions.push((m.start(), CommandKind::Sentence));
            }

            positions.sort_by_key(|p| p.0);

            if let Some((pos, kind)) = positions.first() {
                let pos = *pos;
                match kind {
                    CommandKind::SetVar => {
                        let caps = RE_SET_VAR.captures(buffer).unwrap();
                        let mat = caps.get(0).unwrap();
                        let key = caps.get(1).unwrap().as_str().to_string();
                        let value = caps.get(2).unwrap().as_str().to_string();

                        let prefix = buffer[..pos].to_string();
                        if !prefix.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                prefix,
                                play_id.to_string(),
                                None,
                            ));
                        }

                        if let Some(call) = &self.call {
                            let mut state = call.call_state.write().await;
                            let mut extras = state.extras.take().unwrap_or_default();
                            extras.insert(key, serde_json::Value::String(value));
                            state.extras = Some(extras);
                        }

                        buffer.drain(..mat.end());
                    }
                    CommandKind::Http => {
                        let caps = RE_HTTP.captures(buffer).unwrap();
                        let mat = caps.get(0).unwrap();
                        let url = caps.get(1).unwrap().as_str().to_string();
                        let method = caps
                            .get(2)
                            .map(|m| m.as_str().to_string())
                            .unwrap_or("GET".to_string());
                        let body = caps.get(3).map(|m| m.as_str().to_string());

                        // Flush TTS
                        let prefix = buffer[..pos].to_string();
                        if !prefix.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                prefix,
                                play_id.to_string(),
                                None,
                            ));
                        }

                        // Execute HTTP request synchronously and capture response
                        let client = self.client.clone();
                        let mut req = match method.to_uppercase().as_str() {
                            "POST" => client.post(&url),
                            "PUT" => client.put(&url),
                            _ => client.get(&url),
                        };

                        if let Some(b) = body {
                            req = req.body(b);
                        }

                        // Send request and wait for response
                        match req.send().await {
                            Ok(res) => {
                                let status = res.status();
                                let text = res.text().await.unwrap_or_default();
                                info!(url, method, status=?status, "HTTP command executed from stream");

                                // Add response to history for LLM context
                                self.history.push(ChatMessage {
                                    role: "system".to_string(),
                                    content: format!(
                                        "HTTP {} {} returned ({}): {}",
                                        method, url, status, text
                                    ),
                                });
                            }
                            Err(e) => {
                                warn!(
                                    url,
                                    method, "Failed to execute HTTP command from stream: {}", e
                                );

                                // Add error to history
                                self.history.push(ChatMessage {
                                    role: "system".to_string(),
                                    content: format!("HTTP {} {} failed: {}", method, url, e),
                                });
                            }
                        }

                        buffer.drain(..mat.end());
                    }
                    CommandKind::Hangup => {
                        // Don't execute hangup immediately, store it for later
                        // This allows set_var commands after hangup to still be processed
                        let prefix = buffer[..pos].to_string();
                        let hangup_match = RE_HANGUP.find(buffer).unwrap();
                        pending_hangup = Some((prefix, hangup_match.end()));
                        buffer.drain(..hangup_match.end());

                        // Continue processing remaining buffer for set_var commands
                        // Don't return yet!
                    }
                    CommandKind::Refer => {
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
                    CommandKind::Play => {
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
                    CommandKind::Goto => {
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
                        if let Some(scene) = self.scenes.get(&scene_id).cloned() {
                            self.current_scene_id = Some(scene_id);
                            // Dynamically render scene prompt with the latest variables
                            let rendered_prompt = self.render_scene_prompt(&scene).await;
                            // Update system prompt in history
                            let system_prompt = Self::build_system_prompt(
                                &self.config,
                                Some(&rendered_prompt),
                                self.dtmf_collectors.as_ref(),
                            );
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
                    CommandKind::Collect => {
                        let caps = RE_COLLECT.captures(buffer).unwrap();
                        let mat = caps.get(0).unwrap();
                        let collector_type = caps.get(1).unwrap().as_str().to_string();
                        let var_name = caps.get(2).unwrap().as_str().to_string();
                        let prompt = caps.get(3).map(|m| m.as_str().to_string());

                        // Flush any text before the <collect> tag as TTS
                        let prefix = buffer[..pos].to_string();
                        if !prefix.trim().is_empty() {
                            commands.push(self.create_tts_command_with_id(
                                prefix,
                                play_id.to_string(),
                                None,
                            ));
                        }

                        // Play the collector prompt if provided
                        if let Some(p) = prompt {
                            if !p.trim().is_empty() {
                                commands.push(self.create_tts_command(p, None, None));
                            }
                        }

                        // Start the collector
                        if !self.start_collector(&collector_type, &var_name) {
                            // Collector type not found, notify LLM
                            self.history.push(ChatMessage {
                                role: "system".to_string(),
                                content: format!(
                                    "[Unknown DTMF collector type '{}'. Available types: {}]",
                                    collector_type,
                                    self.dtmf_collectors
                                        .as_ref()
                                        .map(|c| c.keys().cloned().collect::<Vec<_>>().join(", "))
                                        .unwrap_or_default()
                                ),
                            });
                        }

                        buffer.drain(..mat.end());
                    }
                    CommandKind::Sentence => {
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
                }
            } else {
                break;
            }
        }

        // Process pending hangup after all other commands (especially set_var)
        if let Some((prefix, _)) = pending_hangup {
            let headers = self.render_sip_headers().await;

            if let Some(call) = &self.call {
                let h_val = serde_json::to_value(&headers).unwrap_or_default();
                let mut state = call.call_state.write().await;
                let mut extras = state.extras.take().unwrap_or_default();
                extras.insert("_hangup_headers".to_string(), h_val);
                state.extras = Some(extras);
            }

            if !prefix.trim().is_empty() {
                let mut cmd =
                    self.create_tts_command_with_id(prefix, play_id.to_string(), Some(true));
                if let Command::Tts { end_of_stream, .. } = &mut cmd {
                    *end_of_stream = Some(true);
                }
                self.is_hanging_up = true;
                commands.push(cmd);
            } else {
                let mut cmd = self.create_tts_command_with_id(
                    "".to_string(),
                    play_id.to_string(),
                    Some(true),
                );
                if let Command::Tts { end_of_stream, .. } = &mut cmd {
                    *end_of_stream = Some(true);
                }
                self.is_hanging_up = true;
                commands.push(cmd);
            }

            return commands;
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

            if let Some(last) = commands.last_mut() {
                if let Command::Tts { end_of_stream, .. } = last {
                    *end_of_stream = Some(true);
                }
            } else if !self.is_hanging_up {
                commands.push(Command::Tts {
                    text: "".to_string(),
                    speaker: None,
                    play_id: Some(play_id.to_string()),
                    auto_hangup: None,
                    streaming: Some(true),
                    end_of_stream: Some(true),
                    option: None,
                    wait_input_timeout: None,
                    base64: None,
                    cache_key: None,
                });
            }
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
            cache_key: None,
        }
    }

    async fn handle_tool_invocation(
        &mut self,
        tool: ToolInvocation,
        tool_commands: &mut Vec<Command>,
    ) -> Result<bool> {
        match tool {
            ToolInvocation::Hangup {
                ref reason,
                ref initiator,
            } => {
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

                let headers = self.render_sip_headers().await;

                tool_commands.push(Command::Hangup {
                    reason: reason.clone(),
                    initiator: initiator.clone(),
                    headers,
                });
                Ok(false)
            }
            ToolInvocation::Refer {
                ref caller,
                ref callee,
                ref options,
            } => {
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
                Ok(false)
            }
            ToolInvocation::Rag {
                ref query,
                ref source,
            } => {
                self.handle_rag_tool(query, source).await?;
                Ok(true)
            }
            ToolInvocation::Accept { ref options } => {
                self.send_debug_event("tool_invocation", json!({ "tool": "Accept" }));
                tool_commands.push(Command::Accept {
                    option: options.clone().unwrap_or_default(),
                });
                Ok(false)
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
                Ok(false)
            }
            ToolInvocation::Http {
                ref url,
                ref method,
                ref body,
                ref headers,
            } => {
                self.handle_http_tool(url, method, body, headers).await?;
                Ok(true)
            }
        }
    }

    async fn render_sip_headers(&self) -> Option<HashMap<String, String>> {
        let hangup_template = self.sip_config.as_ref()?.hangup_headers.as_ref()?;
        let call = self.call.as_ref()?;
        let state = call.call_state.read().await;

        let mut context = HashMap::new();
        let mut sip_headers = HashMap::new();

        // Get the list of SIP header keys stored during extraction
        // If not present, sip dict will be empty (no headers were configured for extraction)
        let sip_header_keys: Vec<String> = state
            .extras
            .as_ref()
            .and_then(|e| e.get("_sip_header_keys"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        if let Some(extras) = &state.extras {
            for (k, v) in extras {
                // Skip internal keys
                if k.starts_with('_') {
                    continue;
                }
                context.insert(k.clone(), v.clone());
                // Only include keys that were extracted as SIP headers
                if sip_header_keys.contains(k) {
                    sip_headers.insert(k.clone(), v.clone());
                }
            }
        }

        // Add sip dictionary for template access
        context.insert(
            "sip".to_string(),
            serde_json::to_value(&sip_headers).unwrap_or(serde_json::Value::Null),
        );

        let env = minijinja::Environment::new();
        let mut rendered_headers = HashMap::new();
        for (k, v) in hangup_template {
            if let Ok(rendered) = env.render_str(v, &context) {
                rendered_headers.insert(k.clone(), rendered);
            } else {
                rendered_headers.insert(k.clone(), v.clone());
            }
        }
        Some(rendered_headers)
    }

    async fn handle_rag_tool(&mut self, query: &str, source: &Option<String>) -> Result<()> {
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

        let rag_result = self.rag_retriever.retrieve(query).await?;

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

        Ok(())
    }

    async fn handle_http_tool(
        &mut self,
        url: &str,
        method: &Option<String>,
        body: &Option<serde_json::Value>,
        headers: &Option<HashMap<String, String>>,
    ) -> Result<()> {
        let method_str = method.as_deref().unwrap_or("GET").to_uppercase();
        let method =
            reqwest::Method::from_bytes(method_str.as_bytes()).unwrap_or(reqwest::Method::GET);

        self.send_debug_event(
            "tool_invocation",
            json!({
                "tool": "Http",
                "params": {
                    "url": url,
                    "method": method_str,
                }
            }),
        );

        let mut req = self.client.request(method, url);
        if let Some(body) = body {
            req = req.json(body);
        }
        if let Some(headers) = headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        match req.send().await {
            Ok(res) => {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                self.history.push(ChatMessage {
                    role: "system".to_string(),
                    content: format!("HTTP tool response ({}): {}", status, text),
                });
            }
            Err(e) => {
                warn!("HTTP tool failed: {}", e);
                self.history.push(ChatMessage {
                    role: "system".to_string(),
                    content: format!("HTTP tool failed: {}", e),
                });
            }
        }

        Ok(())
    }

    async fn handle_asr_final(&mut self, text: &str) -> Result<Vec<Command>> {
        if text.trim().is_empty() {
            return Ok(vec![]);
        }

        self.apply_context_repair(text);
        self.apply_rolling_summary().await;

        self.last_asr_final_at = Some(std::time::Instant::now());
        self.last_interaction_at = std::time::Instant::now();
        self.is_speaking = false;
        self.consecutive_follow_ups = 0;

        self.generate_response().await
    }

    fn apply_context_repair(&mut self, text: &str) {
        let enable_repair = self
            .config
            .features
            .as_ref()
            .map(|f| f.contains(&"context_repair".to_string()))
            .unwrap_or(false);

        if !enable_repair {
            self.history.push(ChatMessage {
                role: "user".to_string(),
                content: text.to_string(),
            });
            return;
        }

        let repair_window_ms = self.config.repair_window_ms.unwrap_or(3000) as u128;
        let mut merged = false;

        if let Some(last_robot_at) = self.last_robot_msg_at {
            if last_robot_at.elapsed().as_millis() < repair_window_ms {
                if let Some(last_msg) = self.history.last() {
                    if last_msg.role == "assistant" && last_msg.content.chars().count() < 15 {
                        info!(
                            "Context Repair: Detected potential fragmentation. Triggering merge."
                        );
                        self.history.pop();
                        if let Some(prev_user) = self.history.last_mut() {
                            if prev_user.role == "user" {
                                prev_user.content.push_str("，");
                                prev_user.content.push_str(text);
                                merged = true;
                            }
                        }
                    }
                }
            }
        }

        if !merged {
            self.history.push(ChatMessage {
                role: "user".to_string(),
                content: text.to_string(),
            });
        }
    }

    async fn apply_rolling_summary(&mut self) {
        let enable_summary = self
            .config
            .features
            .as_ref()
            .map(|f| f.contains(&"rolling_summary".to_string()))
            .unwrap_or(false);

        if !enable_summary {
            return;
        }

        let summary_limit = self.config.summary_limit.unwrap_or(20);
        if self.history.len() <= summary_limit {
            return;
        }

        info!("Rolling Summary: History limit reached. Triggering background summary.");
        let keep_recent = 6;
        if self.history.len() <= summary_limit + keep_recent
            || self.history.len() <= keep_recent + 1
        {
            return;
        }

        let split_idx = self.history.len() - keep_recent;
        let to_summarize = self.history[1..split_idx].to_vec();
        let recent = self.history[split_idx..].to_vec();

        let summary_prompt =
            "Summarize the above conversation so far, focusing on key details and user intent.";
        let mut summary_req_history = to_summarize;
        summary_req_history.push(ChatMessage {
            role: "user".to_string(),
            content: summary_prompt.to_string(),
        });

        match self.provider.call(&self.config, &summary_req_history).await {
            Ok(summary) => {
                let mut new_history = Vec::new();
                if let Some(sys) = self.history.first() {
                    let mut new_sys = sys.clone();
                    new_sys.content.push_str("\n\n[Previous Context Summary]: ");
                    new_sys.content.push_str(&summary);
                    new_history.push(new_sys);
                }
                new_history.extend(recent);
                self.history = new_history;
                info!(
                    "Rolling Summary: Applied summary. New history len: {}",
                    self.history.len()
                );
            }
            Err(e) => {
                warn!("Rolling Summary failed: {}", e);
            }
        }
    }

    fn check_interruption(
        &mut self,
        event: &SessionEvent,
        is_filler: &Option<bool>,
    ) -> Option<Command> {
        let strategy = self.interruption_config.strategy;
        let should_check = match (strategy, event) {
            (InterruptionStrategy::None, _) => false,
            (InterruptionStrategy::Vad, SessionEvent::Speaking { .. }) => true,
            (InterruptionStrategy::Asr, SessionEvent::AsrDelta { .. }) => true,
            (InterruptionStrategy::Both, _) => true,
            _ => false,
        };

        if !self.is_speaking || self.is_hanging_up || !should_check {
            return None;
        }

        // Protection period check
        if let Some(last_start) = self.last_tts_start_at {
            let ignore_ms = self.interruption_config.ignore_first_ms.unwrap_or(800);
            if last_start.elapsed().as_millis() < ignore_ms as u128 {
                return None;
            }
        }

        // Filler word filter
        if self.interruption_config.filler_word_filter.unwrap_or(false) {
            if let Some(true) = is_filler {
                return None;
            }
            if let SessionEvent::AsrDelta { text, .. } = event {
                if is_likely_filler(text) {
                    return None;
                }
            }
        }

        // Stale event check
        if let Some(last_final) = self.last_asr_final_at {
            if last_final.elapsed().as_millis() < 500 {
                return None;
            }
        }

        info!("Smart interruption detected, stopping playback");
        self.is_speaking = false;
        Some(Command::Interrupt {
            graceful: Some(true),
            fade_out_ms: self.interruption_config.volume_fade_ms,
        })
    }

    async fn handle_silence(&mut self) -> Result<Vec<Command>> {
        let follow_up_config = if let Some(scene_id) = &self.current_scene_id {
            self.scenes
                .get(scene_id)
                .and_then(|s| s.follow_up)
                .or(self.global_follow_up_config)
        } else {
            self.global_follow_up_config
        };

        let Some(config) = follow_up_config else {
            return Ok(vec![]);
        };

        if self.is_speaking
            || self.last_interaction_at.elapsed().as_millis() < config.timeout as u128
        {
            return Ok(vec![]);
        }

        if self.consecutive_follow_ups >= config.max_count {
            info!("Max follow-up count reached, hanging up");
            let headers = self.render_sip_headers().await;
            return Ok(vec![Command::Hangup {
                reason: Some("Max follow-up reached".to_string()),
                initiator: Some("system".to_string()),
                headers,
            }]);
        }

        info!(
            "Silence timeout detected ({}ms), triggering follow-up ({}/{})",
            self.last_interaction_at.elapsed().as_millis(),
            self.consecutive_follow_ups + 1,
            config.max_count
        );
        self.consecutive_follow_ups += 1;
        self.last_interaction_at = std::time::Instant::now();
        self.generate_response().await
    }

    async fn handle_function_call(&mut self, name: &str, arguments: &str) -> Result<Vec<Command>> {
        info!(
            "Function call from Realtime: {} with args {}",
            name, arguments
        );
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();

        match name {
            "hangup_call" => {
                let headers = self.render_sip_headers().await;
                Ok(vec![Command::Hangup {
                    reason: args["reason"].as_str().map(|s| s.to_string()),
                    initiator: Some("ai".to_string()),
                    headers,
                }])
            }
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

    async fn interpret_response(&mut self, initial: String) -> Result<Vec<Command>> {
        let mut tool_commands = Vec::new();
        let mut wait_input_timeout = None;
        let mut attempts = 0;
        let mut raw = initial;

        let final_text = loop {
            attempts += 1;

            let Some(structured) = parse_structured_response(&raw) else {
                break Some(raw);
            };

            if wait_input_timeout.is_none() {
                wait_input_timeout = structured.wait_input_timeout;
            }

            let mut rerun_for_rag = false;
            if let Some(tools) = structured.tools {
                for tool in tools {
                    let needs_rerun = self
                        .handle_tool_invocation(tool, &mut tool_commands)
                        .await?;
                    rerun_for_rag = rerun_for_rag || needs_rerun;
                }
            }

            if !rerun_for_rag {
                break structured.text;
            }

            if attempts >= MAX_RAG_ATTEMPTS {
                warn!("Reached RAG iteration limit, using last response");
                break structured.text.or(Some(raw));
            }

            raw = self.call_llm().await?;
        };

        let has_hangup = tool_commands
            .iter()
            .any(|c| matches!(c, Command::Hangup { .. }));
        let mut commands = Vec::new();

        if let Some(text) = final_text {
            if !text.trim().is_empty() {
                self.history.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: text.clone(),
                });
                self.last_tts_start_at = Some(std::time::Instant::now());
                self.is_speaking = true;

                let auto_hangup = has_hangup.then_some(true);
                commands.push(self.create_tts_command(text, wait_input_timeout, auto_hangup));

                if has_hangup {
                    tool_commands.retain(|c| !matches!(c, Command::Hangup { .. }));
                    self.is_hanging_up = true;
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
        // When in DTMF collection mode, only handle DTMF events and track lifecycle
        if self.collector_state.is_some() {
            match event {
                SessionEvent::Dtmf { digit, .. } => {
                    info!("DTMF received (collecting): {}", digit);
                    return self.handle_collector_digit(digit).await;
                }
                SessionEvent::Silence { .. } => {
                    // Check collector timeout on silence events
                    return self.check_collector_timeout().await;
                }
                SessionEvent::TrackEnd { .. } => {
                    self.is_speaking = false;
                    return Ok(vec![]);
                }
                SessionEvent::TrackStart { .. } => {
                    self.is_speaking = true;
                    return Ok(vec![]);
                }
                SessionEvent::Hangup { .. } => {
                    // Allow hangup to pass through
                    self.collector_state = None;
                }
                // Ignore ASR/Speaking/Eou during collection (not interruptible by default)
                SessionEvent::AsrFinal { .. }
                | SessionEvent::AsrDelta { .. }
                | SessionEvent::Speaking { .. }
                | SessionEvent::Eou { .. } => {
                    let interruptible = self
                        .collector_state
                        .as_ref()
                        .and_then(|s| s.config.interruptible)
                        .unwrap_or(false);
                    if !interruptible {
                        return Ok(vec![]);
                    }
                    // If interruptible, fall through to normal handling
                }
                _ => return Ok(vec![]),
            }
        }

        match event {
            SessionEvent::Dtmf { digit, .. } => {
                info!("DTMF received: {}", digit);
                if let Some(action) = self.get_dtmf_action(digit) {
                    self.handle_dtmf_action(action).await
                } else {
                    Ok(vec![])
                }
            }
            SessionEvent::AsrFinal { text, .. } => self.handle_asr_final(text).await,
            SessionEvent::AsrDelta { is_filler, .. } | SessionEvent::Speaking { is_filler, .. } => {
                Ok(self
                    .check_interruption(event, is_filler)
                    .into_iter()
                    .collect())
            }
            SessionEvent::Eou { completed, .. } => {
                if *completed && !self.is_speaking {
                    info!("EOU detected, triggering early response");
                    self.generate_response().await
                } else {
                    Ok(vec![])
                }
            }
            SessionEvent::Silence { .. } => self.handle_silence().await,
            SessionEvent::TrackStart { .. } => {
                self.is_speaking = true;
                Ok(vec![])
            }
            SessionEvent::TrackEnd { .. } => {
                self.is_speaking = false;
                self.is_hanging_up = false;
                self.last_interaction_at = std::time::Instant::now();
                Ok(vec![])
            }
            SessionEvent::FunctionCall {
                name, arguments, ..
            } => self.handle_function_call(name, arguments).await,
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
