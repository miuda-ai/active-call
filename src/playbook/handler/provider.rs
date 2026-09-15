use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::Stream;
use reqwest::Client;
use serde_json::json;
use std::pin::Pin;

use super::super::{LlmConfig, ChatMessage};
use super::types::ToolInvocation;

#[derive(Debug, Clone)]
pub enum LlmStreamEvent {
    Content(String),
    Reasoning(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn call(&self, config: &LlmConfig, history: &[ChatMessage]) -> Result<String>;
    async fn call_stream(
        &self,
        config: &LlmConfig,
        history: &[ChatMessage],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LlmStreamEvent>> + Send>>>;
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

pub struct DefaultLlmProvider {
    client: Client,
}

impl DefaultLlmProvider {
    pub fn new() -> Self {
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
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LlmStreamEvent>> + Send>>> {
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
                                    if let Some(delta) = json["choices"][0].get("delta") {
                                         if let Some(thinking) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                                             yield Ok(LlmStreamEvent::Reasoning(thinking.to_string()));
                                         }
                                         if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                                             yield Ok(LlmStreamEvent::Content(content.to_string()));
                                         }
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
