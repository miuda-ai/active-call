use crate::media::{ambiance::AmbianceOption, recorder::RecorderFormat};
use crate::useragent::RegisterOption;
use anyhow::{Error, Result};
use clap::Parser;
use rustrtc::IceServer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Parser, Debug)]
#[command(version)]
pub struct Cli {
    #[clap(long)]
    pub conf: Option<String>,

    #[clap(long)]
    pub http: Option<String>,

    #[clap(long)]
    pub sip: Option<String>,
}

pub(crate) fn default_config_recorder_path() -> String {
    #[cfg(target_os = "windows")]
    return "./config/recorders".to_string();
    #[cfg(not(target_os = "windows"))]
    return "./config/recorders".to_string();
}

fn default_config_media_cache_path() -> String {
    #[cfg(target_os = "windows")]
    return "./config/mediacache".to_string();
    #[cfg(not(target_os = "windows"))]
    return "./config/mediacache".to_string();
}

fn default_config_http_addr() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_sip_addr() -> String {
    "0.0.0.0".to_string()
}

fn default_sip_port() -> u16 {
    25060
}

fn default_config_rtp_start_port() -> Option<u16> {
    Some(12000)
}

fn default_config_rtp_end_port() -> Option<u16> {
    Some(42000)
}

fn default_codecs() -> Option<Vec<String>> {
    let mut codecs = vec![
        "pcmu".to_string(),
        "pcma".to_string(),
        "g722".to_string(),
        "g729".to_string(),
        "telephone_event".to_string(),
    ];

    #[cfg(feature = "opus")]
    {
        codecs.push("opus".to_string());
    }

    Some(codecs)
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct RecordingPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_start: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samplerate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptime: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<RecorderFormat>,
}

impl RecordingPolicy {
    pub fn recorder_path(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .unwrap_or_else(default_config_recorder_path)
    }

    pub fn recorder_format(&self) -> RecorderFormat {
        self.format.unwrap_or_default()
    }

    pub fn ensure_defaults(&mut self) -> bool {
        if self
            .path
            .as_ref()
            .map(|p| p.trim().is_empty())
            .unwrap_or(true)
        {
            self.path = Some(default_config_recorder_path());
        }

        false
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RewriteRule {
    pub r#match: String,
    pub rewrite: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_config_http_addr")]
    pub http_addr: String,
    pub addr: String,
    pub udp_port: u16,

    pub log_level: Option<String>,
    pub log_file: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_access_skip_paths: Vec<String>,

    pub useragent: Option<String>,
    pub register_users: Option<Vec<RegisterOption>>,
    pub graceful_shutdown: Option<bool>,
    pub handler: Option<InviteHandlerConfig>,
    pub accept_timeout: Option<String>,
    #[serde(default = "default_codecs")]
    pub codecs: Option<Vec<String>>,
    pub external_ip: Option<String>,
    #[serde(default = "default_config_rtp_start_port")]
    pub rtp_start_port: Option<u16>,
    #[serde(default = "default_config_rtp_end_port")]
    pub rtp_end_port: Option<u16>,

    pub callrecord: Option<CallRecordConfig>,
    #[serde(default = "default_config_media_cache_path")]
    pub media_cache_path: String,
    pub ambiance: Option<AmbianceOption>,
    pub ice_servers: Option<Vec<IceServer>>,
    #[serde(default)]
    pub recording: Option<RecordingPolicy>,
    pub rewrites: Option<Vec<RewriteRule>>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type")]
pub enum InviteHandlerConfig {
    Webhook {
        url: Option<String>,
        urls: Option<Vec<String>>,
        method: Option<String>,
        headers: Option<Vec<(String, String)>>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum S3Vendor {
    Aliyun,
    Tencent,
    Minio,
    AWS,
    GCP,
    Azure,
    DigitalOcean,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum CallRecordConfig {
    Local {
        root: String,
    },
    S3 {
        vendor: S3Vendor,
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        endpoint: String,
        root: String,
        with_media: Option<bool>,
        keep_media_copy: Option<bool>,
    },
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
        with_media: Option<bool>,
        keep_media_copy: Option<bool>,
    },
}

impl Default for CallRecordConfig {
    fn default() -> Self {
        Self::Local {
            #[cfg(target_os = "windows")]
            root: "./config/cdr".to_string(),
            #[cfg(not(target_os = "windows"))]
            root: "./config/cdr".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: default_config_http_addr(),
            log_level: None,
            log_file: None,
            http_access_skip_paths: Vec::new(),
            addr: default_sip_addr(),
            udp_port: default_sip_port(),
            useragent: None,
            register_users: None,
            graceful_shutdown: Some(true),
            handler: None,
            accept_timeout: Some("50s".to_string()),
            media_cache_path: default_config_media_cache_path(),
            ambiance: None,
            callrecord: None,
            ice_servers: None,
            codecs: None,
            external_ip: None,
            rtp_start_port: default_config_rtp_start_port(),
            rtp_end_port: default_config_rtp_end_port(),
            recording: None,
            rewrites: None,
        }
    }
}

impl Clone for Config {
    fn clone(&self) -> Self {
        // This is a bit expensive but Config is not cloned often in hot paths
        // and implementing Clone manually for all nested structs is tedious
        let s = toml::to_string(self).unwrap();
        toml::from_str(&s).unwrap()
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Error> {
        let config: Self = toml::from_str(
            &std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {}", e, path))?,
        )?;
        Ok(config)
    }

    pub fn recorder_path(&self) -> String {
        self.recording
            .as_ref()
            .map(|policy| policy.recorder_path())
            .unwrap_or_else(default_config_recorder_path)
    }

    pub fn recorder_format(&self) -> RecorderFormat {
        self.recording
            .as_ref()
            .map(|policy| policy.recorder_format())
            .unwrap_or_default()
    }

    pub fn ensure_recording_defaults(&mut self) -> bool {
        let mut fallback = false;

        if let Some(policy) = self.recording.as_mut() {
            fallback |= policy.ensure_defaults();
        }

        fallback
    }
}
