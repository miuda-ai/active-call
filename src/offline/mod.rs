pub mod config;
pub mod downloader;

#[cfg(feature = "offline")]
pub mod sensevoice;

#[cfg(feature = "offline")]
pub mod supertonic;

pub use config::OfflineConfig;
pub use downloader::{ModelDownloader, ModelType};

#[cfg(feature = "offline")]
pub use sensevoice::SensevoiceEncoder;

#[cfg(feature = "offline")]
pub use supertonic::SupertonicTts;

use anyhow::{Result, anyhow};
use once_cell::sync::OnceCell;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

#[cfg(feature = "offline")]
pub struct OfflineModels {
    config: OfflineConfig,
    sensevoice: Arc<RwLock<Option<SensevoiceEncoder>>>,
    supertonic: Arc<RwLock<Option<SupertonicTts>>>,
}

#[cfg(feature = "offline")]
impl OfflineModels {
    pub fn new(config: OfflineConfig) -> Self {
        Self {
            config,
            sensevoice: Arc::new(RwLock::new(None)),
            supertonic: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn init_sensevoice(&self) -> Result<()> {
        let mut guard = self.sensevoice.write().await;
        if guard.is_none() {
            if !self.config.sensevoice_available() {
                anyhow::bail!(
                    "SenseVoice model files not found. Please run with --download-models sensevoice"
                );
            }

            info!("Initializing SenseVoice encoder...");
            let encoder = SensevoiceEncoder::new(
                &self.config.sensevoice_model_path(),
                &self.config.sensevoice_tokens_path(),
                self.config.threads,
            )?;
            *guard = Some(encoder);
            info!("✓ SenseVoice encoder initialized");
        }
        Ok(())
    }

    pub async fn get_sensevoice(&self) -> Result<Arc<RwLock<Option<SensevoiceEncoder>>>> {
        self.init_sensevoice().await?;
        Ok(self.sensevoice.clone())
    }

    pub async fn init_supertonic(&self) -> Result<()> {
        let mut guard = self.supertonic.write().await;
        if guard.is_none() {
            if !self.config.supertonic_available() {
                anyhow::bail!(
                    "Supertonic model files not found. Please run with --download-models supertonic"
                );
            }

            info!("Initializing Supertonic TTS...");
            let tts = SupertonicTts::new(
                &self.config.supertonic_onnx_dir(),
                &self.config.supertonic_config_path(),
                &self.config.supertonic_voice_styles_dir(),
                self.config.threads,
                false, // use_gpu
            )?;
            *guard = Some(tts);
            info!("✓ Supertonic TTS initialized");
        }
        Ok(())
    }

    pub async fn get_supertonic(&self) -> Result<Arc<RwLock<Option<SupertonicTts>>>> {
        self.init_supertonic().await?;
        Ok(self.supertonic.clone())
    }

    pub fn config(&self) -> &OfflineConfig {
        &self.config
    }
}

#[cfg(feature = "offline")]
static OFFLINE_MODELS: OnceCell<OfflineModels> = OnceCell::new();

#[cfg(feature = "offline")]
pub fn init_offline_models(config: OfflineConfig) -> Result<()> {
    debug!(
        "Initializing offline models with dir: {}",
        config.models_dir.display()
    );
    OFFLINE_MODELS
        .set(OfflineModels::new(config))
        .map_err(|_| anyhow!("offline models already initialized"))
}

#[cfg(feature = "offline")]
pub fn get_offline_models() -> Option<&'static OfflineModels> {
    OFFLINE_MODELS.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_paths() {
        let config = OfflineConfig::default();
        assert!(
            config
                .sensevoice_dir()
                .to_string_lossy()
                .contains("sensevoice")
        );
        assert!(
            config
                .supertonic_dir()
                .to_string_lossy()
                .contains("supertonic")
        );
    }
}
