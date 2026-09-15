use anyhow::Result;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct OfflineConfig {
    pub models_dir: PathBuf,
    pub threads: usize,
}

impl Default for OfflineConfig {
    fn default() -> Self {
        Self {
            models_dir: Self::default_models_dir(),
            threads: num_cpus::get().min(4),
        }
    }
}

impl OfflineConfig {
    pub fn new(models_dir: PathBuf, threads: usize) -> Self {
        Self {
            models_dir,
            threads,
        }
    }

    pub fn default_models_dir() -> PathBuf {
        std::env::var("OFFLINE_MODELS_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./models"))
    }

    pub fn sensevoice_dir(&self) -> PathBuf {
        self.models_dir.join("sensevoice")
    }

    pub fn sensevoice_model_path(&self) -> PathBuf {
        self.sensevoice_dir().join("model.int8.onnx")
    }

    pub fn sensevoice_tokens_path(&self) -> PathBuf {
        self.sensevoice_dir().join("tokens.txt")
    }

    pub fn supertonic_dir(&self) -> PathBuf {
        self.models_dir.join("supertonic")
    }

    pub fn supertonic_onnx_dir(&self) -> PathBuf {
        self.supertonic_dir().join("onnx")
    }

    pub fn supertonic_config_path(&self) -> PathBuf {
        self.supertonic_onnx_dir().join("tts.json")
    }

    pub fn supertonic_voice_styles_dir(&self) -> PathBuf {
        self.supertonic_dir().join("voice_styles")
    }

    pub fn validate(&self) -> Result<()> {
        if !self.models_dir.exists() {
            anyhow::bail!(
                "Models directory does not exist: {}. Please run with --download-models flag.",
                self.models_dir.display()
            );
        }
        Ok(())
    }

    pub fn sensevoice_available(&self) -> bool {
        self.sensevoice_model_path().exists() && self.sensevoice_tokens_path().exists()
    }

    pub fn supertonic_available(&self) -> bool {
        let onnx_dir = self.supertonic_onnx_dir();
        onnx_dir.join("duration_predictor.onnx").exists()
            && onnx_dir.join("text_encoder.onnx").exists()
            && onnx_dir.join("vector_estimator.onnx").exists()
            && onnx_dir.join("vocoder.onnx").exists()
            && self.supertonic_config_path().exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = OfflineConfig::default();
        assert!(config.threads > 0);
        assert!(config.threads <= 4);
    }

    #[test]
    fn test_paths() {
        let config = OfflineConfig::new(PathBuf::from("/test/models"), 2);
        assert_eq!(
            config.sensevoice_model_path(),
            PathBuf::from("/test/models/sensevoice/model.int8.onnx")
        );
        assert_eq!(
            config.supertonic_onnx_dir(),
            PathBuf::from("/test/models/supertonic/onnx")
        );
    }
}
