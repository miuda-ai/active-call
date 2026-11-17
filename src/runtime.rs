//! Live runtime state derived from the configuration snapshot.
//!
//! `AppStateInner` holds the hot-swappable `Config`, but the media engine's
//! processor hooks are built once and only receive a `TrackConfig`, so the
//! module switches they need live here in a process-wide `ArcSwap`. The
//! reload path (`AppStateInner::reload_config`) updates both atomically
//! enough for our purposes: a call reads whatever snapshot is current when it
//! is set up.
//!
//! Precedence for a module switch, highest first:
//!   1. the ops toggle file (`PBX_TOGGLE_FILE`, default `/app/.env`) — the
//!      emergency switch operators already use, read per call;
//!   2. `[modules]` in the active-call config file (hot-reloaded);
//!   3. enabled.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;

use crate::config::{Config, RuntimeModule};

/// Effective module switches for the current config snapshot.
#[derive(Debug, Clone)]
pub struct ModuleSwitches {
    pub denoise: bool,
    pub vad: bool,
    pub eou: bool,
    pub ringback_detection: bool,
    pub sipflow: bool,
    pub playbooks: bool,
}

impl Default for ModuleSwitches {
    fn default() -> Self {
        Self {
            denoise: true,
            vad: true,
            eou: true,
            ringback_detection: true,
            sipflow: true,
            playbooks: true,
        }
    }
}

impl ModuleSwitches {
    pub fn from_config(config: &Config) -> Self {
        Self {
            denoise: config.module_enabled(RuntimeModule::Denoise),
            vad: config.module_enabled(RuntimeModule::Vad),
            eou: config.module_enabled(RuntimeModule::Eou),
            ringback_detection: config.module_enabled(RuntimeModule::RingbackDetection),
            sipflow: config.module_enabled(RuntimeModule::Sipflow),
            playbooks: config.module_enabled(RuntimeModule::Playbooks),
        }
    }
}

static MODULE_SWITCHES: Lazy<ArcSwap<ModuleSwitches>> =
    Lazy::new(|| ArcSwap::from_pointee(ModuleSwitches::default()));

/// Current module switches (cheap `Arc` clone; taken once per call setup).
pub fn module_switches() -> Arc<ModuleSwitches> {
    MODULE_SWITCHES.load_full()
}

/// Publish the switches derived from a freshly loaded config. Called at
/// startup and on every successful reload.
pub fn apply_config(config: &Config) {
    MODULE_SWITCHES.store(Arc::new(ModuleSwitches::from_config(config)));
}

/// Ops-managed toggle file. The deploy mounts the env file into the
/// container; overridable via `PBX_TOGGLE_FILE` for tests/embedders.
pub fn toggle_file_path() -> PathBuf {
    std::env::var("PBX_TOGGLE_FILE")
        .unwrap_or_else(|_| "/app/.env".to_string())
        .into()
}

/// Value of `key` in the toggle file: `Some(false)` when it is explicitly
/// disabled, `Some(true)` when explicitly enabled, `None` when absent (so the
/// caller's configured default applies).
pub fn toggle_value(key: &str) -> Option<bool> {
    toggle_value_from(&toggle_file_path(), key)
}

/// Same as [`toggle_value`] but against an explicit path (tests).
/// `KEY=false|0|off` disables; `KEY=true|1|on` enables; anything else is
/// treated as absent. Lines starting with `#` are ignored.
pub fn toggle_value_from(path: &Path, key: &str) -> Option<bool> {
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{}=", key);
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix(&prefix) {
            let value = value.trim().trim_matches('"').to_ascii_lowercase();
            return match value.as_str() {
                "false" | "0" | "off" => Some(false),
                "true" | "1" | "on" => Some(true),
                _ => None,
            };
        }
    }
    None
}

/// Resolve a module switch: toggle-file override first, then the value from
/// the current config snapshot.
pub fn module_override(key: &str, configured: bool) -> bool {
    toggle_value(key).unwrap_or(configured)
}

#[cfg(test)]
mod tests {
    use super::{ModuleSwitches, toggle_value_from};
    use crate::config::{AdmissionConfig, Config, ModulesConfig, ModuleToggle, RuntimeModule};

    fn temp_toggle_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ac_runtime_toggle_{}_{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("app.env")
    }

    #[test]
    fn toggle_file_values() {
        let path = temp_toggle_file("values");
        std::fs::remove_file(&path).ok();
        // Missing file: nothing is specified.
        assert_eq!(toggle_value_from(&path, "DENOISE_ENABLED"), None);

        std::fs::write(&path, "RUST_LOG=info\nDENOISE_ENABLED=false\n").unwrap();
        assert_eq!(toggle_value_from(&path, "DENOISE_ENABLED"), Some(false));
        assert_eq!(toggle_value_from(&path, "EOU_ENABLED"), None);

        std::fs::write(&path, "DENOISE_ENABLED=\"on\"\n").unwrap();
        assert_eq!(toggle_value_from(&path, "DENOISE_ENABLED"), Some(true));

        std::fs::write(&path, "DENOISE_ENABLED=maybe\n").unwrap();
        assert_eq!(toggle_value_from(&path, "DENOISE_ENABLED"), None);
    }

    #[test]
    fn modules_come_from_config_with_defaults() {
        let mut config = Config::default();
        assert!(config.module_enabled(RuntimeModule::Denoise));

        config.modules = Some(ModulesConfig {
            denoise: Some(ModuleToggle { enabled: false }),
            ..Default::default()
        });
        assert!(!config.module_enabled(RuntimeModule::Denoise));
        assert!(config.module_enabled(RuntimeModule::Vad));

        let switches = ModuleSwitches::from_config(&config);
        assert!(!switches.denoise);
        assert!(switches.sipflow);
    }

    #[test]
    fn admission_defaults() {
        let config = Config::default();
        assert_eq!(config.admission_max_calls(), 0);
        assert_eq!(config.admission_wait_ms(), 5_000);

        let mut config = Config::default();
        config.admission = Some(AdmissionConfig {
            max_active_calls: Some(120),
            wait_ms: Some(1500),
        });
        assert_eq!(config.admission_max_calls(), 120);
        assert_eq!(config.admission_wait_ms(), 1500);
    }
}
