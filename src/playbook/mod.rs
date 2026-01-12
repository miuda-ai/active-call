use crate::EouOption;
use crate::media::recorder::RecorderOption;
use crate::media::vad::VADOption;
use crate::synthesis::SynthesisOption;
use crate::transcription::TranscriptionOption;
use anyhow::{Result, anyhow};
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
pub struct PlaybookConfig {
    pub asr: Option<TranscriptionOption>,
    pub tts: Option<SynthesisOption>,
    pub llm: Option<LlmConfig>,
    pub vad: Option<VADOption>,
    pub denoise: Option<bool>,
    pub recorder: Option<RecorderOption>,
    pub extra: Option<HashMap<String, String>>,
    pub eou: Option<EouOption>,
    pub greeting: Option<String>,
    pub interruption: Option<InterruptionStrategy>,
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
}

#[derive(Debug, Clone)]
pub struct Playbook {
    pub config: PlaybookConfig,
}

impl Playbook {
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path).await?;
        if !content.starts_with("---") {
            return Err(anyhow!("Missing front matter"));
        }

        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Err(anyhow!("Invalid front matter format"));
        }

        let yaml_str = parts[1];
        let prompt = parts[2].trim().to_string();

        let mut config: PlaybookConfig = serde_yaml::from_str(yaml_str)?;
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
            llm.prompt = Some(prompt);
        }

        Ok(Self { config })
    }
}

pub mod dialogue;
pub mod handler;
pub mod runner;

pub use dialogue::DialogueHandler;
pub use handler::{LlmHandler, RagRetriever};
pub use runner::PlaybookRunner;
