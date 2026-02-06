use anyhow::Result;
use rsipstack::dialog::{authenticate::Credential, invitation::InviteOption};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

use crate::{
    media::{
        ambiance::AmbianceOption, recorder::RecorderOption, track::media_pass::MediaPassOption,
        vad::VADOption,
    },
    synthesis::SynthesisOption,
    transcription::TranscriptionOption,
};

pub mod app;
pub mod call;
pub mod callrecord;
pub mod config;
pub mod event;
pub mod handler;
pub mod locator;
pub mod media;
pub mod net_tool;

#[cfg(feature = "offline")]
pub mod offline;

pub mod playbook;
pub mod synthesis;
pub mod transcription;
pub mod useragent;

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[serde(default)]
pub struct SipOption {
    pub username: Option<String>,
    pub password: Option<String>,
    pub realm: Option<String>,
    pub contact: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub hangup_headers: Option<HashMap<String, String>>,
    pub extract_headers: Option<Vec<String>>,
}

#[skip_serializing_none]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CallOption {
    pub denoise: Option<bool>,
    pub offer: Option<String>,
    pub callee: Option<String>,
    pub caller: Option<String>,
    pub recorder: Option<RecorderOption>,
    pub vad: Option<VADOption>,
    pub asr: Option<TranscriptionOption>,
    pub tts: Option<SynthesisOption>,
    pub media_pass: Option<MediaPassOption>,
    // handshake timeout in seconds
    pub handshake_timeout: Option<u64>,
    pub enable_ipv6: Option<bool>,
    pub inactivity_timeout: Option<u64>, // inactivity timeout in seconds
    pub sip: Option<SipOption>,
    pub extra: Option<HashMap<String, String>>,
    pub codec: Option<String>, // pcmu, pcma, g722, pcm, only for websocket call
    pub ambiance: Option<AmbianceOption>,
    pub eou: Option<EouOption>,
    pub realtime: Option<RealtimeOption>,
    pub subscribe: Option<bool>,
}

impl Default for CallOption {
    fn default() -> Self {
        Self {
            denoise: None,
            offer: None,
            callee: None,
            caller: None,
            recorder: None,
            asr: None,
            vad: None,
            tts: None,
            media_pass: None,
            handshake_timeout: None,
            inactivity_timeout: Some(50), // default 50 seconds
            enable_ipv6: None,
            sip: None,
            extra: None,
            codec: None,
            ambiance: None,
            eou: None,
            realtime: None,
            subscribe: None,
        }
    }
}

impl CallOption {
    pub fn check_default(&mut self) {
        if let Some(tts) = &mut self.tts {
            tts.check_default();
        }
        if let Some(asr) = &mut self.asr {
            asr.check_default();
        }
        if let Some(realtime) = &mut self.realtime {
            realtime.check_default();
        }
    }

    pub fn build_invite_option(&self) -> Result<InviteOption> {
        let mut invite_option = InviteOption::default();
        if let Some(offer) = &self.offer {
            invite_option.offer = Some(offer.clone().into());
        }
        if let Some(callee) = &self.callee {
            invite_option.callee = callee.clone().try_into()?;
        }
        let caller_uri = if let Some(caller) = &self.caller {
            // Ensure caller URI has proper sip: scheme
            if caller.starts_with("sip:") || caller.starts_with("sips:") {
                caller.clone()
            } else {
                format!("sip:{}", caller)
            }
        } else if let Some(username) = self.sip.as_ref().and_then(|sip| sip.username.as_ref()) {
            // If caller is not specified but we have SIP credentials, use username as caller
            // If realm is available, use it, otherwise use local IP
            let domain = self
                .sip
                .as_ref()
                .and_then(|sip| sip.realm.as_ref())
                .map(|s| s.as_str())
                .unwrap_or("127.0.0.1");
            format!("sip:{}@{}", username, domain)
        } else {
            // Default to a valid SIP URI if nothing is specified
            "sip:active-call@127.0.0.1".to_string()
        };
        invite_option.caller = caller_uri.try_into()?;

        if let Some(sip) = &self.sip {
            invite_option.credential = Some(Credential {
                username: sip.username.clone().unwrap_or_default(),
                password: sip.password.clone().unwrap_or_default(),
                realm: sip.realm.clone(),
            });
            invite_option.headers = sip.headers.as_ref().map(|h| {
                h.iter()
                    .map(|(k, v)| rsip::Header::Other(k.clone(), v.clone()))
                    .collect::<Vec<_>>()
            });
            sip.contact.as_ref().map(|c| match c.clone().try_into() {
                Ok(u) => {
                    invite_option.contact = u;
                }
                Err(_) => {}
            });
        }
        Ok(invite_option)
    }
}

#[skip_serializing_none]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ReferOption {
    pub denoise: Option<bool>,
    pub timeout: Option<u32>,
    pub moh: Option<String>,
    pub asr: Option<TranscriptionOption>,
    /// hangup after the call is ended
    pub auto_hangup: Option<bool>,
    pub sip: Option<SipOption>,
    pub call_id: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EouOption {
    pub r#type: Option<String>,
    pub endpoint: Option<String>,
    #[serde(alias = "apiKey")]
    pub secret_key: Option<String>,
    pub secret_id: Option<String>,
    /// max timeout in milliseconds
    pub timeout: Option<u32>,
    pub extra: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Hash, Eq, PartialEq)]
pub enum RealtimeType {
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "azure")]
    Azure,
    Other(String),
}

impl<'de> Deserialize<'de> for RealtimeType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "openai" => Ok(RealtimeType::OpenAI),
            "azure" => Ok(RealtimeType::Azure),
            _ => Ok(RealtimeType::Other(value)),
        }
    }
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeOption {
    pub provider: Option<RealtimeType>,
    pub model: Option<String>,
    #[serde(alias = "apiKey")]
    pub secret_key: Option<String>,
    pub secret_id: Option<String>,
    pub endpoint: Option<String>,
    pub turn_detection: Option<serde_json::Value>,
    pub tools: Option<Vec<serde_json::Value>>,
    pub extra: Option<HashMap<String, String>>,
}

impl RealtimeOption {
    pub fn check_default(&mut self) {
        if self.secret_key.is_none() {
            self.secret_key = std::env::var("OPENAI_API_KEY").ok();
        }
    }
}

pub type Spawner = fn(
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
) -> tokio::task::JoinHandle<()>;
static EXTERNAL_SPAWNER: std::sync::OnceLock<Spawner> = std::sync::OnceLock::new();

pub fn set_spawner(spawner: Spawner) -> Result<(), Spawner> {
    EXTERNAL_SPAWNER.set(spawner)
}

pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Some(spawner) = EXTERNAL_SPAWNER.get() {
        spawner(Box::pin(future))
    } else {
        tokio::spawn(future)
    }
}
