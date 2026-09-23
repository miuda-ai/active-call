use crate::media::{ambiance::AmbianceOption, recorder::RecorderFormat};
use crate::useragent::RegisterOption;
use anyhow::{Error, Result};
use clap::Parser;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::rsip::uri::{Auth, HostWithPort, Uri};
use rustrtc::IceServer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Parser, Debug)]
#[command(version)]
pub struct Cli {
    /// Path to configuration file
    #[clap(long)]
    pub conf: Option<String>,
    /// HTTP listening address
    #[clap(long)]
    pub http: Option<String>,

    /// SIP listening port
    #[clap(long)]
    pub sip: Option<String>,

    /// SIP invitation handler: URL for webhook (http://...) or playbook file (.md)
    #[clap(long)]
    pub handler: Option<String>,

    /// Call a SIP address immediately and use the handler for the call
    #[clap(long)]
    pub call: Option<String>,

    /// External IP address for SIP/RTP
    #[clap(long)]
    pub external_ip: Option<String>,

    /// Supported codecs (e.g., pcmu,pcma,g722,g729,opus)
    #[clap(long, value_delimiter = ',')]
    pub codecs: Option<Vec<String>>,

    /// Download models (sensevoice, supertonic, or all)
    #[cfg(feature = "offline")]
    #[clap(long)]
    pub download_models: Option<String>,

    /// Models directory for offline inference
    #[cfg(feature = "offline")]
    #[clap(long, default_value = "./models")]
    pub models_dir: String,

    /// Exit after downloading models
    #[cfg(feature = "offline")]
    #[clap(long)]
    pub exit_after_download: bool,
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

fn default_config_rtp_latching() -> Option<bool> {
    Some(true)
}

fn default_graceful_shutdown() -> Option<bool> {
    Some(true)
}

fn default_graceful_shutdown_timeout() -> Option<u64> {
    Some(30)
}

fn default_config_useragent() -> Option<String> {
    Some(format!(
        "active-call({} miuda.ai)",
        env!("CARGO_PKG_VERSION")
    ))
}

fn default_enable_options_response() -> Option<bool> {
    Some(true)
}

fn default_options_allow_registered_servers() -> Option<bool> {
    Some(true)
}

fn default_options_auto_learn() -> Option<bool> {
    Some(true)
}

fn default_options_learn_ttl() -> Option<String> {
    Some("7d".to_string())
}

fn default_codecs() -> Option<Vec<String>> {
    let codecs = vec![
        "pcmu".to_string(),
        "pcma".to_string(),
        "g722".to_string(),
        "g729".to_string(),
        "opus".to_string(),
        "telephone_event".to_string(),
    ];
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
    /// Record at the source's native sample rate instead of resampling to
    /// 16 kHz. The actual rate is detected from the caller leg's codec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_samplerate: Option<bool>,
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

/// Trunk-like SIP INVITE/REFER rewriting rule.
///
/// Rules are evaluated in declaration order and the first rule whose `match`
/// conditions all hold (AND) is applied via its `rewrite` actions
/// (first-match-wins). A rule with an empty `match` section always matches and
/// therefore acts as a catch-all default.
///
/// ```toml
/// [[trunk_rules]]
/// rule.match.to.host = "^172\\.25\\."
/// rule.rewrite.contact.host = "172.25.225.2"
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TrunkRule {
    pub rule: TrunkRuleDef,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TrunkRuleDef {
    #[serde(default, rename = "match")]
    pub r#match: TrunkMatch,
    pub rewrite: TrunkRewrite,
}

/// Match conditions for a trunk rule. All non-None fields must match (AND).
/// `from` matches the SIP From header (caller), `to` matches the SIP To header
/// and Request-URI (callee) of the outgoing INVITE/REFER.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct TrunkMatch {
    pub from: Option<UriMatch>,
    pub to: Option<UriMatch>,
}

/// Matches a URI against optional regex patterns on its user and host parts.
/// `user` matches the username before `@`; `host` matches the host after `@`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct UriMatch {
    /// Regex matched against the SIP URI user part (before `@`).
    pub user: Option<String>,
    /// Regex matched against the SIP URI host part (after `@`).
    pub host: Option<String>,
}

/// Rewrite actions applied once a rule matches. All non-None fields are applied.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct TrunkRewrite {
    pub from: Option<UriRewrite>,
    pub to: Option<UriRewrite>,
    pub contact: Option<UriRewrite>,
}

/// Rewrites the user and/or host of a SIP URI. When a `host` value carries no
/// port (e.g. `"172.25.225.2"`), the original URI port is preserved; include a
/// port (e.g. `"172.25.225.2:15060"`) to also change it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct UriRewrite {
    pub user: Option<String>,
    pub host: Option<String>,
}

impl TrunkRule {
    fn matches(&self, invite: &InviteOption) -> bool {
        self.rule.r#match.matches(invite)
    }

    fn apply(&self, invite: &mut InviteOption) {
        self.rule.rewrite.apply(invite);
    }
}

impl TrunkMatch {
    fn matches(&self, invite: &InviteOption) -> bool {
        let from_ok = self
            .from
            .as_ref()
            .map(|from| from.matches(&invite.caller))
            .unwrap_or(true);
        let to_ok = self
            .to
            .as_ref()
            .map(|to| to.matches(&invite.callee))
            .unwrap_or(true);
        from_ok && to_ok
    }
}

impl UriMatch {
    fn matches(&self, uri: &Uri) -> bool {
        let user_ok = self
            .user
            .as_ref()
            .map(|user| {
                uri.auth
                    .as_ref()
                    .map(|auth| regex_is_match(user, &auth.user))
                    .unwrap_or(false)
            })
            .unwrap_or(true);
        let host_ok = self
            .host
            .as_ref()
            .map(|host| regex_is_match(host, &uri.host_with_port.host.to_string()))
            .unwrap_or(true);
        user_ok && host_ok
    }
}

impl TrunkRewrite {
    fn apply(&self, invite: &mut InviteOption) {
        if let Some(from) = &self.from {
            from.apply(&mut invite.caller);
        }
        if let Some(to) = &self.to {
            to.apply(&mut invite.callee);
        }
        if let Some(contact) = &self.contact {
            contact.apply(&mut invite.contact);
        }
    }
}

impl UriRewrite {
    fn apply(&self, uri: &mut Uri) {
        if let Some(user) = &self.user {
            let auth = uri.auth.get_or_insert_with(|| Auth {
                user: String::new(),
                password: None,
            });
            auth.user = user.clone();
        }
        if let Some(host) = &self.host
            && let Ok(mut host_with_port) = HostWithPort::try_from(host.as_str())
        {
            // Preserve the original port when the rewrite value omits one.
            if host_with_port.port.is_none() {
                host_with_port.port = uri.host_with_port.port;
            }
            uri.host_with_port = host_with_port;
        }
    }
}

fn regex_is_match(pattern: &str, value: &str) -> bool {
    regex::Regex::new(pattern)
        .map(|re| re.is_match(value))
        .unwrap_or(false)
}

/// Extracts the host part of a `register_users[].server` value, which may be
/// `host`, `host:port`, or `[ipv6]:port`.
fn host_of_server(server: &str) -> Option<String> {
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    if let Some(rest) = server.strip_prefix('[') {
        // [ipv6]:port or [ipv6]
        return rest.split(']').next().map(|h| h.to_ascii_lowercase());
    }
    // A bare ipv6 literal without brackets contains multiple colons; keep it whole.
    if server.matches(':').count() > 1 {
        return Some(server.to_ascii_lowercase());
    }
    server.split(':').next().map(|h| h.to_ascii_lowercase())
}

/// One parsed `[options_response].allow` entry: an exact IP, a CIDR block, or
/// a hostname.
#[derive(Debug, Clone, PartialEq)]
pub enum OptionsAclEntry {
    Ip(std::net::IpAddr),
    Cidr(ipnet::IpNet),
    Host(String),
}

impl OptionsAclEntry {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
            return Some(Self::Ip(ip));
        }
        if let Ok(cidr) = raw.parse::<ipnet::IpNet>() {
            return Some(Self::Cidr(cidr));
        }
        Some(Self::Host(raw.to_ascii_lowercase()))
    }

    pub fn matches(&self, source_ip: Option<std::net::IpAddr>, source_host: &str) -> bool {
        match self {
            Self::Ip(ip) => source_ip == Some(*ip),
            Self::Cidr(net) => source_ip.map(|ip| net.contains(&ip)).unwrap_or(false),
            Self::Host(host) => host.eq_ignore_ascii_case(source_host),
        }
    }
}

/// `[options_response]` — customizes the 200 OK answers to out-of-dialog
/// OPTIONS keep-alive probes and restricts which sources are answered.
///
/// A probe is answered only when its source (top Via header) matches the
/// static `allow` ACL, a `register_users[].server` host, or a peer address
/// learned from call traffic (`auto_learn`). Everything else is dropped
/// silently.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct OptionsResponseConfig {
    /// Answer out-of-dialog OPTIONS probes; falls back to the legacy
    /// `enable_options_response` field, defaulting to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Static ACL entries: exact IP (`139.224.72.64`), CIDR (`10.0.0.0/8`),
    /// or hostname (`sip.ccc.aliyuncs.com`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Also answer probes coming from the hosts of `register_users[].server`.
    #[serde(default = "default_options_allow_registered_servers")]
    pub allow_registered_servers: Option<bool>,
    /// Learn peer addresses from call traffic — inbound INVITEs and responses
    /// to our outbound INVITEs — and answer probes from those sources within
    /// `learn_ttl` (strict mode: until the first peer is learned, only the
    /// static ACL and registered hosts apply).
    #[serde(default = "default_options_auto_learn")]
    pub auto_learn: Option<bool>,
    /// How long a learned peer address stays valid, e.g. `"7d"` (default).
    #[serde(default = "default_options_learn_ttl")]
    pub learn_ttl: Option<String>,
    /// Extra headers appended to the OPTIONS 200 OK response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_headers: Vec<(String, String)>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_config_http_addr")]
    pub http_addr: String,
    pub addr: String,
    pub udp_port: u16,
    pub auto_learn_public_address: Option<bool>,

    pub log_level: Option<String>,
    pub log_file: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_access_skip_paths: Vec<String>,

    /// Peer active-call nodes ("ip:port" of their HTTP/WS endpoint).
    ///
    /// When a websocket client connects with a session id that is not hosted on
    /// this node, the originator (empty `forward`) polls each peer once
    /// (`forward=true`) and tunnels the websocket to the peer that hosts the
    /// call. Any request whose `forward` is set must not hop further.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<String>,

    #[serde(default = "default_config_useragent")]
    pub useragent: Option<String>,
    pub register_users: Option<Vec<RegisterOption>>,
    #[serde(default = "default_graceful_shutdown")]
    pub graceful_shutdown: Option<bool>,
    /// Total seconds to wait during graceful shutdown before forcing exit.
    /// SIP de-registration and active call draining run in parallel, both with this timeout.
    #[serde(default = "default_graceful_shutdown_timeout")]
    pub graceful_shutdown_timeout: Option<u64>,
    pub handler: Option<InviteHandlerConfig>,
    pub accept_timeout: Option<String>,
    #[serde(default = "default_codecs")]
    pub codecs: Option<Vec<String>>,
    pub external_ip: Option<String>,
    #[serde(default = "default_config_rtp_start_port")]
    pub rtp_start_port: Option<u16>,
    #[serde(default = "default_config_rtp_end_port")]
    pub rtp_end_port: Option<u16>,
    #[serde(default = "default_config_rtp_latching")]
    pub enable_rtp_latching: Option<bool>,
    pub enable_ice_lite: Option<bool>,
    pub rtp_bind_ip: Option<String>,
    pub tls_port: Option<u16>,
    pub tls_cert_file: Option<String>,
    pub tls_key_file: Option<String>,

    pub enable_srtp: Option<bool>,

    pub callrecord: Option<CallRecordConfig>,
    #[serde(default = "default_config_media_cache_path")]
    pub media_cache_path: String,
    pub ambiance: Option<AmbianceOption>,
    pub ice_servers: Option<Vec<IceServer>>,
    #[serde(default)]
    pub recording: Option<RecordingPolicy>,
    pub rewrites: Option<Vec<RewriteRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trunk_rules: Option<Vec<TrunkRule>>,
    #[serde(default = "default_enable_options_response")]
    pub enable_options_response: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_response: Option<OptionsResponseConfig>,

    /// Follow incoming in-dialog REFER automatically: dial the Refer-To
    /// target and bridge media (transfer). The `transferRequest` session
    /// event is always emitted regardless; when disabled the RFC 3515
    /// implicit subscription is terminated with a 403 NOTIFY instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_refer: Option<bool>,
    /// Timeout in seconds for the INVITE handshake of an auto-refer leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_refer_timeout: Option<u32>,
}

impl Config {
    /// Whether incoming in-dialog REFERs are followed automatically
    /// (defaults to true).
    pub fn auto_refer(&self) -> bool {
        self.auto_refer.unwrap_or(true)
    }

    /// INVITE handshake timeout in seconds for auto-refer legs (default 30).
    pub fn auto_refer_timeout(&self) -> u32 {
        self.auto_refer_timeout.unwrap_or(30)
    }
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
    Playbook {
        rules: Option<Vec<PlaybookRule>>,
        default: Option<String>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PlaybookRule {
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub playbook: String,
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
            peers: Vec::new(),
            addr: default_sip_addr(),
            udp_port: default_sip_port(),
            auto_learn_public_address: None,
            useragent: None,
            register_users: None,
            graceful_shutdown: Some(true),
            graceful_shutdown_timeout: default_graceful_shutdown_timeout(),
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
            enable_rtp_latching: Some(true),
            rtp_bind_ip: None,
            enable_ice_lite: None,
            tls_port: None,
            tls_cert_file: None,
            tls_key_file: None,
            enable_srtp: None,
            recording: None,
            rewrites: None,
            trunk_rules: None,
            enable_options_response: default_enable_options_response(),
            options_response: None,
            auto_refer: None,
            auto_refer_timeout: None,
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

    /// Whether out-of-dialog OPTIONS probes should be answered at all.
    pub fn options_response_enabled(&self) -> bool {
        self.options_response
            .as_ref()
            .and_then(|o| o.enabled)
            .or(self.enable_options_response)
            .unwrap_or(true)
    }

    /// Whether peer addresses should be learned from call traffic.
    pub fn options_auto_learn(&self) -> bool {
        self.options_response
            .as_ref()
            .and_then(|o| o.auto_learn)
            .unwrap_or(true)
    }

    /// TTL for learned peer addresses; unparsable/missing values default to 7 days.
    pub fn options_learn_ttl(&self) -> std::time::Duration {
        const DEFAULT: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);
        self.options_response
            .as_ref()
            .and_then(|o| o.learn_ttl.as_deref())
            .map(|raw| humantime::parse_duration(raw).unwrap_or(DEFAULT))
            .unwrap_or(DEFAULT)
    }

    /// Parsed static ACL entries for OPTIONS probing.
    pub fn options_acl_entries(&self) -> Vec<OptionsAclEntry> {
        self.options_response
            .as_ref()
            .map(|o| {
                o.allow
                    .iter()
                    .filter_map(|raw| OptionsAclEntry::parse(raw))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Hosts of `register_users[].server` when `allow_registered_servers` is
    /// on (the default); empty otherwise.
    pub fn options_registered_hosts(&self) -> Vec<String> {
        let enabled = self
            .options_response
            .as_ref()
            .and_then(|o| o.allow_registered_servers)
            .unwrap_or(true);
        if !enabled {
            return vec![];
        }
        self.register_users
            .as_ref()
            .map(|users| {
                users
                    .iter()
                    .filter_map(|user| host_of_server(&user.server))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Extra headers appended to OPTIONS 200 OK responses.
    pub fn options_extra_headers(&self) -> Vec<(String, String)> {
        self.options_response
            .as_ref()
            .map(|o| o.extra_headers.clone())
            .unwrap_or_default()
    }

    /// Whether `source_ip`/`source_host` matches the static ACL or the
    /// registered-server hosts (the configured, non-learned allow sets).
    pub fn options_matches_static(
        &self,
        source_ip: Option<std::net::IpAddr>,
        source_host: &str,
    ) -> bool {
        self.options_acl_entries()
            .iter()
            .any(|entry| entry.matches(source_ip, source_host))
            || self
                .options_registered_hosts()
                .iter()
                .any(|host| host.eq_ignore_ascii_case(source_host))
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

    pub fn recorder_native_samplerate(&self) -> bool {
        self.recording
            .as_ref()
            .and_then(|policy| policy.native_samplerate)
            .unwrap_or(false)
    }

    pub fn ensure_recording_defaults(&mut self) -> bool {
        let mut fallback = false;

        if let Some(policy) = self.recording.as_mut() {
            fallback |= policy.ensure_defaults();
        }

        fallback
    }

    /// Apply the first matching trunk rule to an outgoing SIP INVITE/REFER.
    ///
    /// Rules are evaluated in declaration order; the first rule whose `match`
    /// conditions (all AND) hold is applied and later rules are skipped
    /// (first-match-wins). A rule with an empty `match` section always matches
    /// and acts as a catch-all default. If no rule matches, the invite is left
    /// untouched. Does nothing when [`Config::trunk_rules`] is not configured.
    pub fn apply_trunk_rules(&self, invite: &mut InviteOption) {
        if let Some(rules) = &self.trunk_rules {
            for rule in rules {
                if rule.matches(invite) {
                    rule.apply(invite);
                    break;
                }
            }
        }
    }

    /// Normalize a configured peer to a `ws://` (or `wss://`) base URL.
    ///
    /// Accepts bare `ip:port`, or an explicit `ws://`, `wss://`, `http://` or
    /// `https://` scheme. Returns `None` when the value is empty or unusable.
    pub fn peer_ws_endpoint(peer: &str) -> Option<String> {
        let peer = peer.trim();
        if peer.is_empty() {
            return None;
        }
        if peer.starts_with("wss://") {
            return Some(peer.to_string());
        }
        if peer.starts_with("ws://") {
            return Some(peer.to_string());
        }
        if peer.starts_with("https://") {
            return Some(peer.replacen("https://", "wss://", 1));
        }
        if peer.starts_with("http://") {
            return Some(peer.replacen("http://", "ws://", 1));
        }
        Some(format!("ws://{}", peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_playbook_handler_config_parsing() {
        let toml_config = r#"
http_addr = "0.0.0.0:8080"
addr = "0.0.0.0"
udp_port = 25060

[handler]
type = "playbook"
default = "default.md"

[[handler.rules]]
caller = "^\\+1\\d{10}$"
callee = "^sip:support@.*"
playbook = "support.md"

[[handler.rules]]
caller = "^\\+86\\d+"
playbook = "chinese.md"

[[handler.rules]]
callee = "^sip:sales@.*"
playbook = "sales.md"
"#;

        let config: Config = toml::from_str(toml_config).unwrap();

        assert!(config.handler.is_some());
        if let Some(InviteHandlerConfig::Playbook { rules, default }) = config.handler {
            assert_eq!(default, Some("default.md".to_string()));
            let rules = rules.unwrap();
            assert_eq!(rules.len(), 3);

            assert_eq!(rules[0].caller, Some(r"^\+1\d{10}$".to_string()));
            assert_eq!(rules[0].callee, Some("^sip:support@.*".to_string()));
            assert_eq!(rules[0].playbook, "support.md");

            assert_eq!(rules[1].caller, Some(r"^\+86\d+".to_string()));
            assert_eq!(rules[1].callee, None);
            assert_eq!(rules[1].playbook, "chinese.md");

            assert_eq!(rules[2].caller, None);
            assert_eq!(rules[2].callee, Some("^sip:sales@.*".to_string()));
            assert_eq!(rules[2].playbook, "sales.md");
        } else {
            panic!("Expected Playbook handler config");
        }
    }

    #[test]
    fn test_playbook_handler_config_without_default() {
        let toml_config = r#"
http_addr = "0.0.0.0:8080"
addr = "0.0.0.0"
udp_port = 25060

[handler]
type = "playbook"

[[handler.rules]]
caller = "^\\+1.*"
playbook = "us.md"
"#;

        let config: Config = toml::from_str(toml_config).unwrap();

        if let Some(InviteHandlerConfig::Playbook { rules, default }) = config.handler {
            assert_eq!(default, None);
            let rules = rules.unwrap();
            assert_eq!(rules.len(), 1);
        } else {
            panic!("Expected Playbook handler config");
        }
    }

    #[test]
    fn test_webhook_handler_config_still_works() {
        let toml_config = r#"
http_addr = "0.0.0.0:8080"
addr = "0.0.0.0"
udp_port = 25060

[handler]
type = "webhook"
url = "http://example.com/webhook"
"#;

        let config: Config = toml::from_str(toml_config).unwrap();

        if let Some(InviteHandlerConfig::Webhook { url, .. }) = config.handler {
            assert_eq!(url, Some("http://example.com/webhook".to_string()));
        } else {
            panic!("Expected Webhook handler config");
        }
    }

    // ---------------------------------------------------------------------------
    // options_response: config parsing + accessors
    // ---------------------------------------------------------------------------

    #[test]
    fn test_options_response_defaults() {
        let config: Config = toml::from_str(
            r#"
http_addr = "0.0.0.0:8080"
addr = "0.0.0.0"
udp_port = 25060
"#,
        )
        .unwrap();

        assert!(config.options_response.is_none());
        // Section absent: enabled via the legacy field/default, auto-learn on,
        // 7d TTL, no static entries, no extra headers.
        assert!(config.options_response_enabled());
        assert!(config.options_auto_learn());
        assert_eq!(
            config.options_learn_ttl(),
            std::time::Duration::from_secs(7 * 24 * 3600)
        );
        assert!(config.options_acl_entries().is_empty());
        assert!(config.options_registered_hosts().is_empty());
        assert!(config.options_extra_headers().is_empty());
    }

    #[test]
    fn test_options_response_config_parsing() {
        let toml_config = r#"
http_addr = "0.0.0.0:8080"
addr = "0.0.0.0"
udp_port = 25060

[options_response]
enabled = true
allow = ["139.224.72.64", "10.0.0.0/8", "SIP.CCC.AliyunCS.com"]
allow_registered_servers = false
auto_learn = false
learn_ttl = "1h"
extra_headers = [["X-Node-Id", "node-1"], ["User-Agent", "ActiveCall-Test"]]
"#;

        let config: Config = toml::from_str(toml_config).unwrap();
        let options = config.options_response.as_ref().unwrap();
        assert_eq!(options.enabled, Some(true));
        assert_eq!(options.allow_registered_servers, Some(false));
        assert_eq!(options.auto_learn, Some(false));

        assert!(!config.options_auto_learn());
        assert_eq!(
            config.options_learn_ttl(),
            std::time::Duration::from_secs(3600)
        );

        let entries = config.options_acl_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            OptionsAclEntry::Ip("139.224.72.64".parse().unwrap())
        );
        assert!(matches!(entries[1], OptionsAclEntry::Cidr(_)));
        assert_eq!(
            entries[2],
            OptionsAclEntry::Host("sip.ccc.aliyuncs.com".to_string())
        );

        let headers = config.options_extra_headers();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0], ("X-Node-Id".to_string(), "node-1".to_string()));
    }

    #[test]
    fn test_options_response_enabled_fallback_chain() {
        // Legacy field only.
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060
enable_options_response = false
"#,
        )
        .unwrap();
        assert!(!config.options_response_enabled());

        // New section wins over the legacy field.
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060
enable_options_response = false

[options_response]
enabled = true
"#,
        )
        .unwrap();
        assert!(config.options_response_enabled());

        // New section with enabled unset falls back to the legacy field.
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060
enable_options_response = false

[options_response]
extra_headers = [["X-A", "1"]]
"#,
        )
        .unwrap();
        assert!(!config.options_response_enabled());
    }

    #[test]
    fn test_options_acl_matching() {
        let toml_config = r#"
addr = "0.0.0.0"
udp_port = 25060

[options_response]
allow = ["139.224.72.64", "10.0.0.0/8", "sip.ccc.aliyuncs.com"]
"#;
        let config: Config = toml::from_str(toml_config).unwrap();
        let ip = |s: &str| Some(s.parse::<std::net::IpAddr>().unwrap());

        // Exact IP.
        assert!(config.options_matches_static(ip("139.224.72.64"), "139.224.72.64"));
        assert!(!config.options_matches_static(ip("139.224.72.65"), "139.224.72.65"));
        // CIDR.
        assert!(config.options_matches_static(ip("10.1.2.3"), "10.1.2.3"));
        assert!(!config.options_matches_static(ip("192.168.1.1"), "192.168.1.1"));
        // Hostname, case-insensitive.
        assert!(config.options_matches_static(None, "SIP.CCC.ALIYUNCS.COM"));
        assert!(!config.options_matches_static(None, "sip.example.com"));
        // An IP source does not match the hostname entry and vice versa.
        assert!(!config.options_matches_static(None, "139.224.72.64"));
    }

    #[test]
    fn test_options_registered_hosts() {
        let toml_config = r#"
addr = "0.0.0.0"
udp_port = 25060

[[register_users]]
server = "sip.example.com:5060"
username = "1001"

[[register_users]]
server = "10.0.0.5"
username = "1002"
"#;
        let config: Config = toml::from_str(toml_config).unwrap();
        // Default: registered servers are allowed.
        assert_eq!(
            config.options_registered_hosts(),
            vec!["sip.example.com".to_string(), "10.0.0.5".to_string()]
        );
        assert!(config.options_matches_static(None, "SIP.EXAMPLE.COM"));
        assert!(config.options_matches_static(Some("10.0.0.5".parse().unwrap()), "10.0.0.5"));

        // Opt out via allow_registered_servers = false.
        let toml_config_off = r#"
addr = "0.0.0.0"
udp_port = 25060

[[register_users]]
server = "sip.example.com"
username = "1001"

[options_response]
allow_registered_servers = false
"#;
        let config: Config = toml::from_str(toml_config_off).unwrap();
        assert!(config.options_registered_hosts().is_empty());
        assert!(!config.options_matches_static(None, "sip.example.com"));
    }

    #[test]
    fn test_host_of_server() {
        assert_eq!(
            host_of_server("sip.example.com:5060"),
            Some("sip.example.com".to_string())
        );
        assert_eq!(
            host_of_server("SIP.Example.COM"),
            Some("sip.example.com".to_string())
        );
        assert_eq!(
            host_of_server("[2001:db8::1]:5060"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(
            host_of_server("2001:db8::1"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(host_of_server(""), None);
    }

    // ---------------------------------------------------------------------------
    // trunk_rules: config parsing
    // ---------------------------------------------------------------------------

    #[test]
    fn test_trunk_rules_config_parsing() {
        let toml_config = r#"
http_addr = "0.0.0.0:8080"
addr = "0.0.0.0"
udp_port = 25060

[[trunk_rules]]
rule.match.to.host = "^172\\.25\\."
rule.rewrite.contact.host = "172.25.225.2"

[[trunk_rules]]
rule.match.from.user = "^\\+86.*"
rule.match.to.host = "^10\\."
rule.rewrite.from.user = "10086"
rule.rewrite.to.host = "10.0.0.1"
rule.rewrite.contact.user = "active-call"
rule.rewrite.contact.host = "10.0.0.1:25060"

[[trunk_rules]]
rule.rewrite.contact.host = "116.62.75.161"
"#;

        let config: Config = toml::from_str(toml_config).unwrap();
        let rules = config.trunk_rules.expect("trunk_rules should be parsed");

        assert_eq!(rules.len(), 3);

        // Rule 1: match to.host, rewrite contact.host
        let m = &rules[0].rule.r#match;
        assert_eq!(m.to.as_ref().unwrap().host.as_deref(), Some("^172\\.25\\."));
        assert_eq!(m.to.as_ref().unwrap().user, None);
        assert_eq!(m.from, None);
        let w = &rules[0].rule.rewrite;
        assert_eq!(
            w.contact.as_ref().unwrap().host.as_deref(),
            Some("172.25.225.2")
        );
        assert_eq!(w.from, None);
        assert_eq!(w.to, None);

        // Rule 2: combined match + all rewrites
        let m = &rules[1].rule.r#match;
        assert_eq!(m.from.as_ref().unwrap().user.as_deref(), Some("^\\+86.*"));
        assert_eq!(m.to.as_ref().unwrap().host.as_deref(), Some("^10\\."));
        let w = &rules[1].rule.rewrite;
        assert_eq!(w.from.as_ref().unwrap().user.as_deref(), Some("10086"));
        assert_eq!(w.to.as_ref().unwrap().host.as_deref(), Some("10.0.0.1"));
        assert_eq!(
            w.contact.as_ref().unwrap().user.as_deref(),
            Some("active-call")
        );
        assert_eq!(
            w.contact.as_ref().unwrap().host.as_deref(),
            Some("10.0.0.1:25060")
        );

        // Rule 3: catch-all (empty match)
        assert_eq!(rules[2].rule.r#match.from, None);
        assert_eq!(rules[2].rule.r#match.to, None);
        assert_eq!(
            rules[2]
                .rule
                .rewrite
                .contact
                .as_ref()
                .unwrap()
                .host
                .as_deref(),
            Some("116.62.75.161")
        );
    }

    #[test]
    fn test_trunk_rules_absent_by_default() {
        let config = Config::default();
        assert!(config.trunk_rules.is_none());
    }

    // ---------------------------------------------------------------------------
    // trunk_rules: match + rewrite behaviour
    // ---------------------------------------------------------------------------

    fn invite_option(caller: &str, callee: &str, contact: &str) -> InviteOption {
        InviteOption {
            caller: caller.try_into().unwrap(),
            callee: callee.try_into().unwrap(),
            contact: contact.try_into().unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn test_trunk_rule_matches_on_to_host() {
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060

[[trunk_rules]]
rule.match.to.host = "^172\\.25\\."
rule.rewrite.contact.host = "172.25.225.2"
"#,
        )
        .unwrap();

        // Internal target -> matched, contact host rewritten.
        let mut invite = invite_option(
            "sip:ai@116.62.75.161:13050",
            "sip:agent1@172.25.225.3:15060",
            "sip:ai@127.0.0.1:13050",
        );
        config.apply_trunk_rules(&mut invite);
        assert_eq!(
            invite.contact.host_with_port.host.to_string(),
            "172.25.225.2"
        );

        // External target -> no match, contact untouched.
        let mut invite = invite_option(
            "sip:ai@116.62.75.161:13050",
            "sip:+8613800138000@sbc.example.com:5060",
            "sip:ai@127.0.0.1:13050",
        );
        config.apply_trunk_rules(&mut invite);
        assert_eq!(invite.contact.host_with_port.host.to_string(), "127.0.0.1");
    }

    #[test]
    fn test_trunk_rule_matches_on_from_user_and_to_host() {
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060

[[trunk_rules]]
rule.match.from.user = "^anonymous$"
rule.match.to.host = "^sbc\\."
rule.rewrite.contact.host = "116.62.75.161"
"#,
        )
        .unwrap();

        // from.user + to.host both match -> rewritten.
        let mut invite = invite_option(
            "sip:anonymous@116.62.75.161:13050",
            "sip:+8613800138000@sbc.example.com:5060",
            "sip:ai@127.0.0.1:13050",
        );
        config.apply_trunk_rules(&mut invite);
        assert_eq!(
            invite.contact.host_with_port.host.to_string(),
            "116.62.75.161"
        );

        // from.user does not match -> no rewrite.
        let mut invite = invite_option(
            "sip:alice@116.62.75.161:13050",
            "sip:+8613800138000@sbc.example.com:5060",
            "sip:ai@127.0.0.1:13050",
        );
        config.apply_trunk_rules(&mut invite);
        assert_eq!(invite.contact.host_with_port.host.to_string(), "127.0.0.1");
    }

    #[test]
    fn test_trunk_rule_rewrites_from_to_contact() {
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060

[[trunk_rules]]
rule.match.to.host = "^10\\."
rule.rewrite.from.user = "10086"
rule.rewrite.from.host = "116.62.75.161"
rule.rewrite.to.user = "30000"
rule.rewrite.to.host = "10.0.0.1:25060"
rule.rewrite.contact.user = "active-call"
rule.rewrite.contact.host = "10.0.0.1"
"#,
        )
        .unwrap();

        let mut invite = invite_option(
            "sip:ai@127.0.0.1:13050",
            "sip:agent1@10.0.0.2:25060",
            "sip:ai@127.0.0.1:13050",
        );
        config.apply_trunk_rules(&mut invite);

        // caller (from)
        assert_eq!(invite.caller.auth.as_ref().unwrap().user, "10086");
        assert_eq!(
            invite.caller.host_with_port.host.to_string(),
            "116.62.75.161"
        );

        // callee (to)
        assert_eq!(invite.callee.auth.as_ref().unwrap().user, "30000");
        assert_eq!(invite.callee.host_with_port.host.to_string(), "10.0.0.1");
        assert_eq!(invite.callee.host_with_port.port.unwrap().0, 25060);

        // contact: host rewritten without port -> original port preserved
        assert_eq!(invite.contact.auth.as_ref().unwrap().user, "active-call");
        assert_eq!(invite.contact.host_with_port.host.to_string(), "10.0.0.1");
        assert_eq!(invite.contact.host_with_port.port.unwrap().0, 13050);
    }

    #[test]
    fn test_trunk_rule_catch_all_default() {
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060

[[trunk_rules]]
rule.match.to.host = "^172\\.25\\."
rule.rewrite.contact.host = "172.25.225.2"

[[trunk_rules]]
rule.rewrite.contact.host = "116.62.75.161"
"#,
        )
        .unwrap();

        // Not matched by rule 1 -> falls through to catch-all rule 2.
        let mut invite = invite_option(
            "sip:ai@116.62.75.161:13050",
            "sip:+8613800138000@sbc.example.com:5060",
            "sip:ai@127.0.0.1:13050",
        );
        config.apply_trunk_rules(&mut invite);
        assert_eq!(
            invite.contact.host_with_port.host.to_string(),
            "116.62.75.161"
        );
    }

    #[test]
    fn test_trunk_rule_first_match_wins() {
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060

[[trunk_rules]]
rule.match.to.host = ".*"
rule.rewrite.contact.host = "1.1.1.1"

[[trunk_rules]]
rule.match.to.host = ".*"
rule.rewrite.contact.host = "2.2.2.2"
"#,
        )
        .unwrap();

        let mut invite = invite_option(
            "sip:ai@116.62.75.161:13050",
            "sip:any@example.com:5060",
            "sip:ai@127.0.0.1:13050",
        );
        config.apply_trunk_rules(&mut invite);
        // Only the first rule applies.
        assert_eq!(invite.contact.host_with_port.host.to_string(), "1.1.1.1");
    }

    #[test]
    fn test_trunk_rule_no_config_noop() {
        let config = Config::default();
        let mut invite = invite_option(
            "sip:ai@116.62.75.161:13050",
            "sip:agent1@172.25.225.3:15060",
            "sip:ai@127.0.0.1:13050",
        );
        let before = invite.contact.to_string();
        config.apply_trunk_rules(&mut invite);
        assert_eq!(invite.contact.to_string(), before);
    }

    #[test]
    fn test_trunk_rule_rewrite_missing_auth_creates_user() {
        let config: Config = toml::from_str(
            r#"
addr = "0.0.0.0"
udp_port = 25060

[[trunk_rules]]
rule.match.to.host = ".*"
rule.rewrite.contact.user = "active-call"
"#,
        )
        .unwrap();

        // A contact without a user part (e.g. "sip:127.0.0.1:13050").
        let mut invite = InviteOption {
            caller: "sip:ai@127.0.0.1:13050".try_into().unwrap(),
            callee: "sip:agent1@172.25.225.3:15060".try_into().unwrap(),
            contact: "sip:127.0.0.1:13050".try_into().unwrap(),
            ..Default::default()
        };
        config.apply_trunk_rules(&mut invite);
        assert_eq!(invite.contact.auth.as_ref().unwrap().user, "active-call");
    }
}
