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
    pub dtmf: Option<HashMap<String, DtmfAction>>,
    pub play: Option<String>,
    pub follow_up: Option<FollowUpConfig>,
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
}
