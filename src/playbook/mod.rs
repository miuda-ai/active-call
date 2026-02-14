use crate::media::recorder::RecorderOption;
use crate::media::vad::VADOption;
use crate::synthesis::SynthesisOption;
use crate::transcription::TranscriptionOption;
use crate::{EouOption, RealtimeOption, SipOption, media::ambiance::AmbianceOption};
use anyhow::{Result, anyhow};
use minijinja::Environment;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, path::Path};
use tokio::fs;

/// Expand environment variables in the format ${VAR_NAME}
fn expand_env_vars(input: &str) -> String {
    let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];
        std::env::var(var_name).unwrap_or_else(|_| format!("${{{}}}", var_name))
    })
    .to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum InterruptionStrategy {
    #[default]
    Both,
    Vad,
    Asr,
    None,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct InterruptionConfig {
    pub strategy: InterruptionStrategy,
    pub min_speech_ms: Option<u32>,
    pub filler_word_filter: Option<bool>,
    pub volume_fade_ms: Option<u32>,
    pub ignore_first_ms: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookConfig {
    pub asr: Option<TranscriptionOption>,
    pub tts: Option<SynthesisOption>,
    pub llm: Option<LlmConfig>,
    pub vad: Option<VADOption>,
    pub denoise: Option<bool>,
    pub ambiance: Option<AmbianceOption>,
    pub recorder: Option<RecorderOption>,
    pub extra: Option<HashMap<String, String>>,
    pub eou: Option<EouOption>,
    pub greeting: Option<String>,
    pub interruption: Option<InterruptionConfig>,
    pub dtmf: Option<HashMap<String, DtmfAction>>,
    pub dtmf_collectors: Option<HashMap<String, DtmfCollectorConfig>>,
    pub realtime: Option<RealtimeOption>,
    pub posthook: Option<PostHookConfig>,
    pub follow_up: Option<FollowUpConfig>,
    pub sip: Option<SipOption>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default)]
#[serde(rename_all = "camelCase")]
pub struct FollowUpConfig {
    pub timeout: u64,
    pub max_count: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum SummaryType {
    Short,
    Detailed,
    Intent,
    Json,
    #[serde(untagged)]
    Custom(String),
}

impl SummaryType {
    pub fn prompt(&self) -> &str {
        match self {
            Self::Short => "summarize the conversation in one or two sentences.",
            Self::Detailed => {
                "summarize the conversation in detail, including key points, decisions, and action items."
            }
            Self::Intent => "identify and summarize the user's main intent and needs.",
            Self::Json => {
                "output the conversation summary in JSON format with fields: intent, key_points, sentiment."
            }
            Self::Custom(p) => p,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PostHookConfig {
    pub url: String,
    pub summary: Option<SummaryType>,
    pub method: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub include_history: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum DtmfAction {
    Goto { scene: String },
    Transfer { target: String },
    Hangup,
}

/// Validation rule for DTMF digit collection
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct DtmfValidation {
    /// Regex pattern for validation, e.g. "^1[3-9]\\d{9}$" for Chinese phone numbers
    pub pattern: String,
    /// Error message shown when validation fails
    pub error_message: Option<String>,
}

/// Configuration for a DTMF digit collector template
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct DtmfCollectorConfig {
    /// Human-readable description of this collector (used in LLM prompt generation)
    pub description: Option<String>,
    /// Exact expected digit count (shorthand for min_digits == max_digits)
    pub digits: Option<u32>,
    /// Minimum digits required
    pub min_digits: Option<u32>,
    /// Maximum digits allowed
    pub max_digits: Option<u32>,
    /// Key that terminates collection: "#" or "*"
    pub finish_key: Option<String>,
    /// Overall timeout in seconds (default: 15)
    pub timeout: Option<u32>,
    /// Max seconds between consecutive key presses (default: 5)
    pub inter_digit_timeout: Option<u32>,
    /// Validation rule (regex + error message)
    pub validation: Option<DtmfValidation>,
    /// Max retry attempts when validation fails (default: 3)
    pub retry_times: Option<u32>,
    /// Whether voice input (ASR) can interrupt collection (default: false)
    pub interruptible: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct LlmConfig {
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub prompt: Option<String>,
    pub greeting: Option<String>,
    pub language: Option<String>,
    pub features: Option<Vec<String>>,
    pub repair_window_ms: Option<u64>,
    pub summary_limit: Option<usize>,
    /// Custom tool instructions. If not set, default tool instructions based on language will be used.
    /// Set this to override the built-in tool usage instructions completely.
    pub tool_instructions: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub id: String,
    pub prompt: String,
    /// The original unrendered prompt template, preserved for dynamic re-rendering
    /// with updated variables (e.g., after set_var during conversation).
    pub raw_prompt: Option<String>,
    pub dtmf: Option<HashMap<String, DtmfAction>>,
    pub play: Option<String>,
    pub follow_up: Option<FollowUpConfig>,
}

/// Built-in session variable key constants.
/// These are automatically injected into `extras` so they can be referenced
/// in playbook templates using `{{ session_id }}`, `{{ call_type }}`, etc.
pub const BUILTIN_SESSION_ID: &str = "session_id";
pub const BUILTIN_CALL_TYPE: &str = "call_type";
pub const BUILTIN_CALLER: &str = "caller";
pub const BUILTIN_CALLEE: &str = "callee";
pub const BUILTIN_START_TIME: &str = "start_time";

/// Render a scene prompt template dynamically using the current variables.
/// This allows `set_var` values set during conversation to be used in scene prompts.
///
/// If `raw_prompt` is `None` or rendering fails, falls back to the pre-rendered `prompt`.
pub fn render_scene_prompt(scene: &Scene, vars: &HashMap<String, serde_json::Value>) -> String {
    let template = match &scene.raw_prompt {
        Some(t) if t.contains("{{") => t,
        _ => return scene.prompt.clone(),
    };

    let env = Environment::new();
    let mut context = vars.clone();

    // Build sip dictionary from _sip_header_keys (same logic as Playbook::render)
    let sip_header_keys: Vec<String> = vars
        .get("_sip_header_keys")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mut sip_headers = HashMap::new();
    for key in &sip_header_keys {
        if let Some(value) = vars.get(key) {
            sip_headers.insert(key.clone(), value.clone());
        }
    }
    context.insert(
        "sip".to_string(),
        serde_json::to_value(&sip_headers).unwrap_or(Value::Null),
    );

    // Remove internal keys from context
    context.retain(|k, _| !k.starts_with('_'));

    match env.render_str(template, &context) {
        Ok(rendered) => rendered,
        Err(_) => scene.prompt.clone(),
    }
}

#[derive(Debug, Clone)]
pub struct Playbook {
    pub raw_content: String,
    pub config: PlaybookConfig,
    pub scenes: HashMap<String, Scene>,
    pub initial_scene_id: Option<String>,
}

impl Playbook {
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path).await?;
        Self::parse(&content)
    }

    pub fn render(&self, vars: &HashMap<String, serde_json::Value>) -> Result<Self> {
        let env = Environment::new();
        let mut context = vars.clone();

        // Get the list of SIP header keys stored by extract_headers processing
        // If not present, sip dict will be empty (no headers were configured for extraction)
        let sip_header_keys: Vec<String> = vars
            .get("_sip_header_keys")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // Separate SIP headers into sip dictionary based on stored keys
        let mut sip_headers = HashMap::new();
        for key in &sip_header_keys {
            if let Some(value) = vars.get(key) {
                sip_headers.insert(key.clone(), value.clone());
            }
        }
        context.insert(
            "sip".to_string(),
            serde_json::to_value(&sip_headers).unwrap_or(Value::Null),
        );

        let rendered = env.render_str(&self.raw_content, &context)?;
        let mut res = Self::parse(&rendered)?;
        // Preserve the original raw_content (with templates) for dynamic re-rendering
        res.raw_content = self.raw_content.clone();
        // Preserve original raw_prompts from the unrendered playbook for dynamic re-rendering
        for (scene_id, scene) in &self.scenes {
            if let Some(res_scene) = res.scenes.get_mut(scene_id) {
                res_scene.raw_prompt = scene.raw_prompt.clone();
            }
        }
        res.config.sip.as_mut().map(|sip| {
            sip.hangup_headers = self
                .config
                .sip
                .as_ref()
                .and_then(|sip| sip.hangup_headers.clone());
        });
        Ok(res)
    }

    pub fn parse(content: &str) -> Result<Self> {
        if !content.starts_with("---") {
            return Err(anyhow!("Missing front matter"));
        }

        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Err(anyhow!("Invalid front matter format"));
        }

        let yaml_str = parts[1];
        let prompt_section = parts[2].trim();

        // Expand environment variables in YAML configuration
        // This allows ALL fields to use ${VAR_NAME} syntax
        let expanded_yaml = expand_env_vars(yaml_str);
        let mut config: PlaybookConfig = serde_yaml::from_str(&expanded_yaml)?;

        let mut scenes = HashMap::new();
        let mut first_scene_id: Option<String> = None;

        let dtmf_regex =
            regex::Regex::new(r#"<dtmf\s+digit="([^"]+)"\s+action="([^"]+)"(?:\s+scene="([^"]+)")?(?:\s+target="([^"]+)")?\s*/>"#).unwrap();
        let play_regex = regex::Regex::new(r#"<play\s+file="([^"]+)"\s*/>"#).unwrap();
        let followup_regex =
            regex::Regex::new(r#"<followup\s+timeout="(\d+)"\s+max="(\d+)"\s*/>"#).unwrap();

        let parse_scene = |id: String, content: String| -> Scene {
            let mut dtmf_map = HashMap::new();
            let mut play = None;
            let mut follow_up = None;
            let mut final_content = content.clone();

            for cap in dtmf_regex.captures_iter(&content) {
                let digit = cap.get(1).unwrap().as_str().to_string();
                let action_type = cap.get(2).unwrap().as_str();

                let action = match action_type {
                    "goto" => {
                        let scene = cap
                            .get(3)
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default();
                        DtmfAction::Goto { scene }
                    }
                    "transfer" => {
                        let target = cap
                            .get(4)
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default();
                        DtmfAction::Transfer { target }
                    }
                    "hangup" => DtmfAction::Hangup,
                    _ => continue,
                };
                dtmf_map.insert(digit, action);
            }

            if let Some(cap) = play_regex.captures(&content) {
                play = Some(cap.get(1).unwrap().as_str().to_string());
            }

            if let Some(cap) = followup_regex.captures(&content) {
                let timeout = cap.get(1).unwrap().as_str().parse().unwrap_or(0);
                let max_count = cap.get(2).unwrap().as_str().parse().unwrap_or(0);
                follow_up = Some(FollowUpConfig { timeout, max_count });
            }

            // Remove dtmf and play tags from the content
            final_content = dtmf_regex.replace_all(&final_content, "").to_string();
            final_content = play_regex.replace_all(&final_content, "").to_string();
            final_content = followup_regex.replace_all(&final_content, "").to_string();
            final_content = final_content.trim().to_string();

            Scene {
                id,
                raw_prompt: Some(final_content.clone()),
                prompt: final_content,
                dtmf: if dtmf_map.is_empty() {
                    None
                } else {
                    Some(dtmf_map)
                },
                play,
                follow_up,
            }
        };

        // Parse scenes from markdown. Look for headers like "# Scene: <id>"
        let scene_regex = regex::Regex::new(r"(?m)^# Scene:\s*(.+)$").unwrap();
        let mut last_match_end = 0;
        let mut last_scene_id: Option<String> = None;

        for cap in scene_regex.captures_iter(prompt_section) {
            let m = cap.get(0).unwrap();
            let scene_id = cap.get(1).unwrap().as_str().trim().to_string();

            if first_scene_id.is_none() {
                first_scene_id = Some(scene_id.clone());
            }

            if let Some(id) = last_scene_id {
                let scene_content = prompt_section[last_match_end..m.start()].trim().to_string();
                scenes.insert(id.clone(), parse_scene(id, scene_content));
            } else {
                // Content before the first scene header
                let pre_content = prompt_section[..m.start()].trim();
                if !pre_content.is_empty() {
                    let id = "default".to_string();
                    first_scene_id = Some(id.clone());
                    scenes.insert(id.clone(), parse_scene(id, pre_content.to_string()));
                }
            }

            last_scene_id = Some(scene_id);
            last_match_end = m.end();
        }

        if let Some(id) = last_scene_id {
            let scene_content = prompt_section[last_match_end..].trim().to_string();
            scenes.insert(id.clone(), parse_scene(id, scene_content));
        } else if !prompt_section.is_empty() {
            // No scene headers found, treat the whole prompt as "default"
            let id = "default".to_string();
            first_scene_id = Some(id.clone());
            scenes.insert(id.clone(), parse_scene(id, prompt_section.to_string()));
        }

        if let Some(llm) = config.llm.as_mut() {
            // Fallback to direct env var if not set
            if llm.api_key.is_none() {
                if let Ok(key) = std::env::var("OPENAI_API_KEY") {
                    llm.api_key = Some(key);
                }
            }
            if llm.base_url.is_none() {
                if let Ok(url) = std::env::var("OPENAI_BASE_URL") {
                    llm.base_url = Some(url);
                }
            }
            if llm.model.is_none() {
                if let Ok(model) = std::env::var("OPENAI_MODEL") {
                    llm.model = Some(model);
                }
            }

            // Use the first scene found as the initial prompt
            if let Some(initial_id) = first_scene_id.clone() {
                if let Some(scene) = scenes.get(&initial_id) {
                    llm.prompt = Some(scene.prompt.clone());
                }
            }
        }

        Ok(Self {
            raw_content: content.to_string(),
            config,
            scenes,
            initial_scene_id: first_scene_id,
        })
    }
}

pub mod dialogue;
pub mod handler;
pub mod runner;

pub use dialogue::DialogueHandler;
pub use handler::{LlmHandler, RagRetriever};
pub use runner::PlaybookRunner;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_playbook_parsing_with_variables() {
        let content = r#"---
llm:
  provider: openai
  model: |-
    {{ model_name }}
  greeting: |-
    Hello, {{ user_name }}!
---
# Scene: main
You are an assistant for {{ company }}.
"#;
        let mut variables = HashMap::new();
        variables.insert("model_name".to_string(), json!("gpt-4"));
        variables.insert("user_name".to_string(), json!("Alice"));
        variables.insert("company".to_string(), json!("RestSend"));

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        assert_eq!(
            playbook.config.llm.as_ref().unwrap().model,
            Some("gpt-4".to_string())
        );
        assert_eq!(
            playbook.config.llm.as_ref().unwrap().greeting,
            Some("Hello, Alice!".to_string())
        );

        let scene = playbook.scenes.get("main").unwrap();
        assert_eq!(scene.prompt, "You are an assistant for RestSend.");
    }

    #[test]
    fn test_playbook_scene_dtmf_parsing() {
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
<dtmf digit="1" action="goto" scene="product" />
<dtmf digit="2" action="transfer" target="sip:123@domain" />
<dtmf digit="0" action="hangup" />
Welcome to our service.
"#;
        let playbook = Playbook::parse(content).unwrap();

        let scene = playbook.scenes.get("main").unwrap();
        assert_eq!(scene.prompt, "Welcome to our service.");

        let dtmf = scene.dtmf.as_ref().unwrap();
        assert_eq!(dtmf.len(), 3);

        match dtmf.get("1").unwrap() {
            DtmfAction::Goto { scene } => assert_eq!(scene, "product"),
            _ => panic!("Expected Goto action"),
        }

        match dtmf.get("2").unwrap() {
            DtmfAction::Transfer { target } => assert_eq!(target, "sip:123@domain"),
            _ => panic!("Expected Transfer action"),
        }

        match dtmf.get("0").unwrap() {
            DtmfAction::Hangup => {}
            _ => panic!("Expected Hangup action"),
        }
    }

    #[test]
    fn test_playbook_dtmf_priority() {
        let content = r#"---
llm:
  provider: openai
dtmf:
  "1": { action: "goto", scene: "global_dest" }
  "9": { action: "hangup" }
---
# Scene: main
<dtmf digit="1" action="goto" scene="local_dest" />
Welcome.
"#;
        let playbook = Playbook::parse(content).unwrap();

        // Check global config
        let global_dtmf = playbook.config.dtmf.as_ref().unwrap();
        assert_eq!(global_dtmf.len(), 2);

        // Check scene config
        let scene = playbook.scenes.get("main").unwrap();
        let scene_dtmf = scene.dtmf.as_ref().unwrap();
        assert_eq!(scene_dtmf.len(), 1);

        // Verify scene has local_dest for "1"
        match scene_dtmf.get("1").unwrap() {
            DtmfAction::Goto { scene } => assert_eq!(scene, "local_dest"),
            _ => panic!("Expected Local Goto action"),
        }
    }

    #[test]
    fn test_posthook_config_parsing() {
        let content = r#"---
posthook:
  url: "http://test.com"
  summary: "json"
  includeHistory: true
  headers:
    X-API-Key: "secret"
llm:
  provider: openai
---
# Scene: main
Hello
"#;
        let playbook = Playbook::parse(content).unwrap();
        let posthook = playbook.config.posthook.unwrap();
        assert_eq!(posthook.url, "http://test.com");
        match posthook.summary.unwrap() {
            SummaryType::Json => {}
            _ => panic!("Expected Json summary type"),
        }
        assert_eq!(posthook.include_history, Some(true));
        assert_eq!(
            posthook.headers.unwrap().get("X-API-Key").unwrap(),
            "secret"
        );
    }

    #[test]
    fn test_env_var_expansion() {
        // Set test env vars
        unsafe {
            std::env::set_var("TEST_API_KEY", "sk-test-12345");
            std::env::set_var("TEST_BASE_URL", "https://api.test.com");
        }

        let content = r#"---
llm:
  provider: openai
  apiKey: "${TEST_API_KEY}"
  baseUrl: "${TEST_BASE_URL}"
  model: gpt-4
---
# Scene: main
Test
"#;
        let playbook = Playbook::parse(content).unwrap();
        let llm = playbook.config.llm.unwrap();

        assert_eq!(llm.api_key.unwrap(), "sk-test-12345");
        assert_eq!(llm.base_url.unwrap(), "https://api.test.com");
        assert_eq!(llm.model.unwrap(), "gpt-4");

        // Clean up
        unsafe {
            std::env::remove_var("TEST_API_KEY");
            std::env::remove_var("TEST_BASE_URL");
        }
    }

    #[test]
    fn test_env_var_expansion_missing() {
        // Test with undefined var
        let content = r#"---
llm:
  provider: openai
  apiKey: "${UNDEFINED_VAR}"
---
# Scene: main
Test
"#;
        let playbook = Playbook::parse(content).unwrap();
        let llm = playbook.config.llm.unwrap();

        // Should keep the placeholder if env var not found
        assert_eq!(llm.api_key.unwrap(), "${UNDEFINED_VAR}");
    }

    #[test]
    fn test_custom_summary_parsing() {
        let content = r#"---
posthook:
  url: "http://test.com"
  summary: "Please summarize customly"
llm:
  provider: openai
---
# Scene: main
Hello
"#;
        let playbook = Playbook::parse(content).unwrap();
        let posthook = playbook.config.posthook.unwrap();
        match posthook.summary.unwrap() {
            SummaryType::Custom(s) => assert_eq!(s, "Please summarize customly"),
            _ => panic!("Expected Custom summary type"),
        }
    }

    #[test]
    fn test_sip_dict_access_with_hyphens() {
        // Test accessing SIP headers with hyphens via sip dictionary
        let content = r#"---
llm:
  provider: openai
  greeting: Hello {{ sip["X-Customer-Name"] }}!
---
# Scene: main
Your ID is {{ sip["X-Customer-ID"] }}.
Session type: {{ sip["X-Session-Type"] }}.
"#;
        let mut variables = HashMap::new();
        variables.insert("X-Customer-Name".to_string(), json!("Alice"));
        variables.insert("X-Customer-ID".to_string(), json!("CID-12345"));
        variables.insert("X-Session-Type".to_string(), json!("inbound"));
        // Simulate extract_headers processing
        variables.insert(
            "_sip_header_keys".to_string(),
            json!(["X-Customer-Name", "X-Customer-ID", "X-Session-Type"]),
        );

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        assert_eq!(
            playbook.config.llm.as_ref().unwrap().greeting,
            Some("Hello Alice!".to_string())
        );

        let scene = playbook.scenes.get("main").unwrap();
        assert_eq!(
            scene.prompt,
            "Your ID is CID-12345.\nSession type: inbound."
        );
    }

    #[test]
    fn test_sip_dict_only_contains_sip_headers() {
        // Test that sip dict only contains SIP headers from extract_headers, not other variables
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
SIP Header: {{ sip["X-Custom-Header"] }}
Regular var: {{ regular_var }}
"#;
        let mut variables = HashMap::new();
        variables.insert("X-Custom-Header".to_string(), json!("header_value"));
        variables.insert("regular_var".to_string(), json!("regular_value"));
        variables.insert("another_var".to_string(), json!("another"));
        // Only X-Custom-Header is extracted
        variables.insert("_sip_header_keys".to_string(), json!(["X-Custom-Header"]));

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        let scene = playbook.scenes.get("main").unwrap();

        // Both should work - SIP header via sip dict, regular var via direct access
        assert!(scene.prompt.contains("SIP Header: header_value"));
        assert!(scene.prompt.contains("Regular var: regular_value"));
    }

    #[test]
    fn test_sip_dict_mixed_access() {
        // Test that both direct access and sip dict access work together
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
Direct: {{ simple_var }}
SIP Header: {{ sip["X-Custom-Header"] }}
SIP via Direct: {{ X_Custom_Header2 }}
"#;
        let mut variables = HashMap::new();
        variables.insert("simple_var".to_string(), json!("direct_value"));
        variables.insert("X-Custom-Header".to_string(), json!("header_value"));
        variables.insert("X_Custom_Header2".to_string(), json!("header2_value"));
        // Only X-Custom-Header is in extract_headers
        variables.insert("_sip_header_keys".to_string(), json!(["X-Custom-Header"]));

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        let scene = playbook.scenes.get("main").unwrap();

        assert!(scene.prompt.contains("Direct: direct_value"));
        assert!(scene.prompt.contains("SIP Header: header_value"));
        // X_Custom_Header2 doesn't start with X-, so won't be in sip dict
        assert!(scene.prompt.contains("SIP via Direct: header2_value"));
    }

    #[test]
    fn test_sip_dict_empty_context() {
        // Test that sip dict works with no variables
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
No variables here.
"#;
        let playbook = Playbook::parse(content).unwrap();
        let scene = playbook.scenes.get("main").unwrap();
        assert_eq!(scene.prompt, "No variables here.");
    }

    #[test]
    fn test_sip_dict_case_insensitive() {
        // Test that extract_headers can include headers with different cases
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
Upper: {{ sip["X-Header-Upper"] }}
Lower: {{ sip["x-header-lower"] }}
"#;
        let mut variables = HashMap::new();
        variables.insert("X-Header-Upper".to_string(), json!("UPPER"));
        variables.insert("x-header-lower".to_string(), json!("lower"));
        variables.insert(
            "_sip_header_keys".to_string(),
            json!(["X-Header-Upper", "x-header-lower"]),
        );

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        let scene = playbook.scenes.get("main").unwrap();

        assert!(scene.prompt.contains("Upper: UPPER"));
        assert!(scene.prompt.contains("Lower: lower"));
    }

    #[test]
    fn test_env_vars_in_all_fields() {
        // Test that ${VAR} works in all configuration fields
        unsafe {
            std::env::set_var("TEST_MODEL_ALL", "gpt-4o");
            std::env::set_var("TEST_API_KEY_ALL", "sk-test-12345");
            std::env::set_var("TEST_BASE_URL_ALL", "https://api.example.com");
            std::env::set_var("TEST_SPEAKER_ALL", "F1");
            std::env::set_var("TEST_LANGUAGE_ALL", "zh");
            std::env::set_var("TEST_SPEED_ALL", "1.2");
        }

        let content = r#"---
asr:
  provider: "sensevoice"
  language: "${TEST_LANGUAGE_ALL}"
tts:
  provider: "supertonic"
  speaker: "${TEST_SPEAKER_ALL}"
  speed: ${TEST_SPEED_ALL}
llm:
  provider: "openai"
  model: "${TEST_MODEL_ALL}"
  apiKey: "${TEST_API_KEY_ALL}"
  baseUrl: "${TEST_BASE_URL_ALL}"
---
# Scene: main
Test content
"#;

        let playbook = Playbook::parse(content).unwrap();

        // Verify ASR fields
        let asr = playbook.config.asr.unwrap();
        assert_eq!(asr.language.unwrap(), "zh");

        // Verify TTS fields
        let tts = playbook.config.tts.unwrap();
        assert_eq!(tts.speaker.unwrap(), "F1");
        assert_eq!(tts.speed, Some(1.2));

        // Verify LLM fields
        let llm = playbook.config.llm.unwrap();
        assert_eq!(llm.model.unwrap(), "gpt-4o");
        assert_eq!(llm.api_key.unwrap(), "sk-test-12345");
        assert_eq!(llm.base_url.unwrap(), "https://api.example.com");

        unsafe {
            std::env::remove_var("TEST_MODEL_ALL");
            std::env::remove_var("TEST_API_KEY_ALL");
            std::env::remove_var("TEST_BASE_URL_ALL");
            std::env::remove_var("TEST_SPEAKER_ALL");
            std::env::remove_var("TEST_LANGUAGE_ALL");
            std::env::remove_var("TEST_SPEED_ALL");
        }
    }

    #[test]
    fn test_sip_dict_with_http_command() {
        // Test that SIP headers work correctly in HTTP command URLs
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
Querying API: <http url='https://api.example.com/customers/{{ sip["X-Customer-ID"] }}' method="GET" />
"#;
        let mut variables = HashMap::new();
        variables.insert("X-Customer-ID".to_string(), json!("CUST12345"));
        variables.insert("_sip_header_keys".to_string(), json!(["X-Customer-ID"]));

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        let scene = playbook.scenes.get("main").unwrap();

        // The HTTP tag should be preserved in the prompt with the variable expanded
        assert!(
            scene
                .prompt
                .contains("https://api.example.com/customers/CUST12345")
        );
    }

    #[test]
    fn test_sip_dict_without_extract_config() {
        // Test that sip dict is empty when no _sip_header_keys is present
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
Regular var: {{ regular_var }}
SIP dict should be empty.
"#;
        let mut variables = HashMap::new();
        variables.insert("regular_var".to_string(), json!("regular_value"));
        // No _sip_header_keys, so sip dict should be empty

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        let scene = playbook.scenes.get("main").unwrap();

        assert!(scene.prompt.contains("Regular var: regular_value"));
    }

    #[test]
    fn test_sip_dict_with_multiple_headers_in_yaml() {
        // Test SIP headers used in YAML configuration section
        let content = r#"---
llm:
  provider: openai
  greeting: 'Welcome {{ sip["X-Customer-Name"] }}! Your ID is {{ sip["X-Customer-ID"] }}.'
---
# Scene: main
How can I help you today?
"#;
        let mut variables = HashMap::new();
        variables.insert("X-Customer-Name".to_string(), json!("Alice"));
        variables.insert("X-Customer-ID".to_string(), json!("CUST789"));
        variables.insert(
            "_sip_header_keys".to_string(),
            json!(["X-Customer-Name", "X-Customer-ID"]),
        );

        let playbook = Playbook::parse(content).unwrap();
        let playbook = playbook.render(&variables).unwrap();

        assert_eq!(
            playbook.config.llm.as_ref().unwrap().greeting,
            Some("Welcome Alice! Your ID is CUST789.".to_string())
        );
    }

    #[test]
    fn test_wrong_syntax_should_fail() {
        // Test that using {{ X-Header }} (without sip dict) should fail
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
This will fail: {{ X-Customer-ID }}
"#;
        let mut variables = HashMap::new();
        variables.insert("X-Customer-ID".to_string(), json!("CUST123"));
        variables.insert("_sip_header_keys".to_string(), json!(["X-Customer-ID"]));

        // This should fail because X-Customer-ID is not in the direct context
        // It's only in sip dict
        let playbook = Playbook::parse(content).unwrap();
        let result = playbook.render(&variables);
        // Templates are not checked during parsing anymore
        assert!(result.is_err());
    }

    #[test]
    fn test_sip_dict_with_set_var() {
        // Test that SIP headers work in set_var commands
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
<set_var key="X-Call-Status" value="active" />
Customer: {{ sip["X-Customer-ID"] }}
Status set successfully.
"#;
        let mut variables = HashMap::new();
        variables.insert("X-Customer-ID".to_string(), json!("CUST456"));
        variables.insert("_sip_header_keys".to_string(), json!(["X-Customer-ID"]));

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();
        let scene = playbook.scenes.get("main").unwrap();

        assert!(scene.prompt.contains("Customer: CUST456"));
        assert!(scene.prompt.contains("<set_var"));
    }

    #[test]
    fn test_sip_dict_mixed_with_regular_vars_in_complex_scenario() {
        // Test complex scenario with both SIP headers and regular variables
        let content = r#"---
llm:
  provider: openai
  greeting: 'Hello {{ sip["X-Customer-Name"] }}, member level: {{ member_level }}'
---
# Scene: main
Your ID: {{ sip["X-Customer-ID"] }}
Your status: {{ account_status }}
Your priority: {{ sip["X-Priority"] }}
Order count: {{ order_count }}
"#;
        let mut variables = HashMap::new();
        // SIP headers
        variables.insert("X-Customer-Name".to_string(), json!("Bob"));
        variables.insert("X-Customer-ID".to_string(), json!("CUST999"));
        variables.insert("X-Priority".to_string(), json!("VIP"));
        // Regular variables
        variables.insert("member_level".to_string(), json!("Gold"));
        variables.insert("account_status".to_string(), json!("Active"));
        variables.insert("order_count".to_string(), json!(5));
        // Mark SIP headers
        variables.insert(
            "_sip_header_keys".to_string(),
            json!(["X-Customer-Name", "X-Customer-ID", "X-Priority"]),
        );

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();

        // Check greeting has both types
        assert_eq!(
            playbook.config.llm.as_ref().unwrap().greeting,
            Some("Hello Bob, member level: Gold".to_string())
        );

        // Check scene has all variables correctly rendered
        let scene = playbook.scenes.get("main").unwrap();
        assert!(scene.prompt.contains("Your ID: CUST999"));
        assert!(scene.prompt.contains("Your status: Active"));
        assert!(scene.prompt.contains("Your priority: VIP"));
        assert!(scene.prompt.contains("Order count: 5"));
    }

    #[test]
    fn test_raw_prompt_preserved_after_render() {
        // Test that raw_prompt is preserved after rendering so it can be re-rendered later
        let content = r#"---
llm:
  provider: openai
---
# Scene: greeting
您好，{{ customer_name }}！您的意图是：{{ intent }}
# Scene: detail
客户意图：{{ intent }}
详细信息在此。
"#;
        let mut variables = HashMap::new();
        variables.insert("customer_name".to_string(), json!("张三"));
        variables.insert("intent".to_string(), json!("咨询"));

        let playbook = Playbook::parse(content)
            .unwrap()
            .render(&variables)
            .unwrap();

        // Verify rendered prompts
        let greeting = playbook.scenes.get("greeting").unwrap();
        assert!(greeting.prompt.contains("您好，张三"));
        assert!(greeting.prompt.contains("您的意图是：咨询"));

        // Verify raw_prompt still has the template
        assert!(greeting.raw_prompt.is_some());
        let raw = greeting.raw_prompt.as_ref().unwrap();
        assert!(raw.contains("{{ customer_name }}"));
        assert!(raw.contains("{{ intent }}"));

        // Verify detail scene too
        let detail = playbook.scenes.get("detail").unwrap();
        assert!(detail.raw_prompt.is_some());
        assert!(detail.raw_prompt.as_ref().unwrap().contains("{{ intent }}"));
    }

    #[test]
    fn test_render_scene_prompt_with_dynamic_vars() {
        // Test render_scene_prompt: simulates set_var updating variables mid-conversation
        let scene = Scene {
            id: "main".to_string(),
            raw_prompt: Some("客户意图：{{ intent }}\n客户ID：{{ sip[\"X-Jobid\"] }}".to_string()),
            prompt: "客户意图：\n客户ID：JOB123".to_string(), // initially rendered
            ..Default::default()
        };

        // Simulate variables after set_var has been called
        let mut vars = HashMap::new();
        vars.insert("intent".to_string(), json!("买零食"));
        vars.insert("X-Jobid".to_string(), json!("JOB123"));
        vars.insert("_sip_header_keys".to_string(), json!(["X-Jobid"]));

        let rendered = render_scene_prompt(&scene, &vars);
        assert!(rendered.contains("客户意图：买零食"));
        assert!(rendered.contains("客户ID：JOB123"));
    }

    #[test]
    fn test_render_scene_prompt_fallback_without_template() {
        // When raw_prompt has no template markers, should return prompt as-is
        let scene = Scene {
            id: "simple".to_string(),
            raw_prompt: Some("你好，欢迎光临".to_string()),
            prompt: "你好，欢迎光临".to_string(),
            ..Default::default()
        };

        let vars = HashMap::new();
        let rendered = render_scene_prompt(&scene, &vars);
        assert_eq!(rendered, "你好，欢迎光临");
    }

    #[test]
    fn test_render_scene_prompt_fallback_no_raw_prompt() {
        // When raw_prompt is None, should return prompt
        let scene = Scene {
            id: "legacy".to_string(),
            prompt: "Hello world".to_string(),
            ..Default::default()
        };

        let vars = HashMap::new();
        let rendered = render_scene_prompt(&scene, &vars);
        assert_eq!(rendered, "Hello world");
    }

    #[test]
    fn test_render_scene_prompt_with_builtin_vars() {
        // Test that built-in session variables work in scene prompts
        let scene = Scene {
            id: "main".to_string(),
            raw_prompt: Some(
                "会话ID：{{ session_id }}\n呼叫类型：{{ call_type }}\n主叫：{{ caller }}\n被叫：{{ callee }}\n开始时间：{{ start_time }}"
                    .to_string(),
            ),
            prompt: String::new(),
            ..Default::default()
        };

        let mut vars = HashMap::new();
        vars.insert(BUILTIN_SESSION_ID.to_string(), json!("sess-12345"));
        vars.insert(BUILTIN_CALL_TYPE.to_string(), json!("sip"));
        vars.insert(BUILTIN_CALLER.to_string(), json!("sip:alice@example.com"));
        vars.insert(BUILTIN_CALLEE.to_string(), json!("sip:bob@example.com"));
        vars.insert(
            BUILTIN_START_TIME.to_string(),
            json!("2026-02-14T10:00:00Z"),
        );

        let rendered = render_scene_prompt(&scene, &vars);
        assert!(rendered.contains("会话ID：sess-12345"));
        assert!(rendered.contains("呼叫类型：sip"));
        assert!(rendered.contains("主叫：sip:alice@example.com"));
        assert!(rendered.contains("被叫：sip:bob@example.com"));
        assert!(rendered.contains("开始时间：2026-02-14T10:00:00Z"));
    }

    #[test]
    fn test_render_scene_prompt_mixed_sip_and_set_var() {
        // Test mixed SIP headers and set_var variables in dynamic rendering
        let scene = Scene {
            id: "main".to_string(),
            raw_prompt: Some(
                "客户：{{ sip[\"X-Customer-Name\"] }}\n意图：{{ intent }}\n会话：{{ session_id }}"
                    .to_string(),
            ),
            prompt: String::new(),
            ..Default::default()
        };

        let mut vars = HashMap::new();
        // SIP header
        vars.insert("X-Customer-Name".to_string(), json!("王五"));
        vars.insert("_sip_header_keys".to_string(), json!(["X-Customer-Name"]));
        // set_var variable
        vars.insert("intent".to_string(), json!("退货"));
        // Built-in variable
        vars.insert(BUILTIN_SESSION_ID.to_string(), json!("sess-99"));

        let rendered = render_scene_prompt(&scene, &vars);
        assert!(rendered.contains("客户：王五"));
        assert!(rendered.contains("意图：退货"));
        assert!(rendered.contains("会话：sess-99"));
    }

    #[test]
    fn test_render_scene_prompt_graceful_on_missing_vars() {
        // When a referenced variable is missing, MiniJinja renders it as empty string
        // The render still succeeds but with empty values
        let scene = Scene {
            id: "main".to_string(),
            raw_prompt: Some("意图：{{ intent }}".to_string()),
            prompt: "意图：（未知）".to_string(), // fallback (not used since rendering succeeds)
            ..Default::default()
        };

        let vars = HashMap::new(); // no intent variable
        let rendered = render_scene_prompt(&scene, &vars);
        // MiniJinja renders missing vars as empty string
        assert_eq!(rendered, "意图：");
    }

    #[test]
    fn test_raw_prompt_set_on_parse() {
        // Verify that raw_prompt is set during initial parsing
        let content = r#"---
llm:
  provider: openai
---
# Scene: main
Hello {{ name }}!
"#;
        let playbook = Playbook::parse(content).unwrap();
        let scene = playbook.scenes.get("main").unwrap();
        assert!(scene.raw_prompt.is_some());
        assert!(scene.raw_prompt.as_ref().unwrap().contains("{{ name }}"));
    }

    #[test]
    fn test_builtin_var_constants() {
        // Verify the built-in variable constant values
        assert_eq!(BUILTIN_SESSION_ID, "session_id");
        assert_eq!(BUILTIN_CALL_TYPE, "call_type");
        assert_eq!(BUILTIN_CALLER, "caller");
        assert_eq!(BUILTIN_CALLEE, "callee");
        assert_eq!(BUILTIN_START_TIME, "start_time");
    }
}
