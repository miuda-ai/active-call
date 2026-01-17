use crate::media::recorder::RecorderOption;
use crate::media::vad::VADOption;
use crate::synthesis::SynthesisOption;
use crate::transcription::TranscriptionOption;
use crate::{EouOption, RealtimeOption, media::ambiance::AmbianceOption};
use anyhow::{Result, anyhow};
use minijinja::Environment;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};
use tokio::fs;

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
    pub config: PlaybookConfig,
    pub scenes: HashMap<String, Scene>,
    pub initial_scene_id: Option<String>,
}

impl Playbook {
    pub async fn load<P: AsRef<Path>>(
        path: P,
        variables: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Self> {
        let content = fs::read_to_string(path).await?;
        Self::parse(&content, variables)
    }

    pub fn parse(
        content: &str,
        variables: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Self> {
        let rendered_content = if let Some(vars) = variables {
            let env = Environment::new();
            env.render_str(content, vars)?
        } else {
            content.to_string()
        };

        if !rendered_content.starts_with("---") {
            return Err(anyhow!("Missing front matter"));
        }

        let parts: Vec<&str> = rendered_content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Err(anyhow!("Invalid front matter format"));
        }

        let yaml_str = parts[1];
        let prompt_section = parts[2].trim();

        let mut config: PlaybookConfig = serde_yaml::from_str(yaml_str)?;

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
  model: {{ model_name }}
  greeting: Hello, {{ user_name }}!
---
# Scene: main
You are an assistant for {{ company }}.
"#;
        let mut variables = HashMap::new();
        variables.insert("model_name".to_string(), json!("gpt-4"));
        variables.insert("user_name".to_string(), json!("Alice"));
        variables.insert("company".to_string(), json!("RestSend"));

        let playbook = Playbook::parse(content, Some(&variables)).unwrap();

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
        let playbook = Playbook::parse(content, None).unwrap();

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
        let playbook = Playbook::parse(content, None).unwrap();

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
        let playbook = Playbook::parse(content, None).unwrap();
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
        let playbook = Playbook::parse(content, None).unwrap();
        let posthook = playbook.config.posthook.unwrap();
        match posthook.summary.unwrap() {
            SummaryType::Custom(s) => assert_eq!(s, "Please summarize customly"),
            _ => panic!("Expected Custom summary type"),
        }
    }
}
