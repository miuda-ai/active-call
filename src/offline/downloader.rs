use anyhow::{Context, Result};
use hf_hub::api::sync::{Api, ApiBuilder};
use hf_hub::{Repo, RepoType};
use std::fs;
use std::path::Path;
use tracing::{info, warn};

pub struct ModelDownloader {
    api: Api,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelType {
    Sensevoice,
    Supertonic,
    All,
}

impl ModelType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "sensevoice" => Some(Self::Sensevoice),
            "supertonic" => Some(Self::Supertonic),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

impl ModelDownloader {
    pub fn new() -> Result<Self> {
        let api = ApiBuilder::from_env()
            .build()
            .context("failed to create HuggingFace API client")?;
        Ok(Self { api })
    }

    pub fn download(&self, model_type: ModelType, dest_dir: &Path) -> Result<()> {
        match model_type {
            ModelType::Sensevoice => self.download_sensevoice(dest_dir),
            ModelType::Supertonic => self.download_supertonic(dest_dir),
            ModelType::All => {
                self.download_sensevoice(dest_dir)?;
                self.download_supertonic(dest_dir)?;
                Ok(())
            }
        }
    }

    fn download_file(
        &self,
        repo_id: &str,
        revision: &str,
        filename: &str,
        dest: &Path,
    ) -> Result<()> {
        if dest.exists() {
            let meta = fs::metadata(dest).context("failed to get metadata")?;
            if meta.len() < 100 && filename.ends_with(".onnx") {
                warn!(
                    "  File {} exists but is too small ({} bytes). Deleting...",
                    filename,
                    meta.len()
                );
                fs::remove_file(dest).context("failed to remove corrupted file")?;
            } else {
                info!("  {} already exists, skipping", filename);
                return Ok(());
            }
        }

        info!("  Downloading {}...", filename);
        let repo = self.api.repo(Repo::with_revision(
            repo_id.to_string(),
            RepoType::Model,
            revision.to_string(),
        ));

        match repo.get(filename) {
            Ok(src_path) => {
                fs::copy(&src_path, dest).with_context(|| {
                    format!(
                        "failed to copy {} to {}",
                        src_path.display(),
                        dest.display()
                    )
                })?;
                info!("  ✓ Downloaded {}", filename);
                Ok(())
            }
            Err(err) => {
                warn!(
                    "  hf-hub failed for {}: {}. Attempting manual download...",
                    filename, err
                );

                // Fallback using curl
                let endpoint =
                    std::env::var("HF_ENDPOINT").unwrap_or("https://huggingface.co".to_string());
                // Handle nested paths in URL
                let url = format!("{}/{}/resolve/{}/{}", endpoint, repo_id, revision, filename);

                let status = std::process::Command::new("curl")
                    .arg("-f") // Fail on 404
                    .arg("-L")
                    .arg("-o")
                    .arg(dest)
                    .arg(&url)
                    .status()
                    .context("failed to execute curl")?;

                if status.success() {
                    info!("  ✓ Downloaded {} (curl fallback)", filename);
                    Ok(())
                } else {
                    anyhow::bail!("Both hf-hub and curl failed to download {}", filename);
                }
            }
        }
    }

    fn download_sensevoice(&self, dest_dir: &Path) -> Result<()> {
        info!("Downloading SenseVoice model...");
        let sensevoice_dir = dest_dir.join("sensevoice");
        fs::create_dir_all(&sensevoice_dir).context("failed to create sensevoice directory")?;

        let repo_id = "csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09";
        let revision = "main";
        let files = vec!["model.int8.onnx", "tokens.txt"];

        for file in files {
            let dest = sensevoice_dir.join(file);
            self.download_file(repo_id, revision, file, &dest)?;
        }

        info!("✓ SenseVoice model downloaded successfully");
        Ok(())
    }

    fn download_supertonic(&self, dest_dir: &Path) -> Result<()> {
        info!("Downloading Supertonic model...");
        let supertonic_dir = dest_dir.join("supertonic");
        fs::create_dir_all(&supertonic_dir).context("failed to create supertonic directory")?;

        let repo_id = "Supertone/supertonic-2";
        let revision = "main";
        let files = vec![
            "onnx/duration_predictor.onnx",
            "onnx/text_encoder.onnx",
            "onnx/vector_estimator.onnx",
            "onnx/vocoder.onnx",
            "onnx/unicode_indexer.json",
            "onnx/tts.json",
            "voice_styles/M1.json",
            "voice_styles/M2.json",
            "voice_styles/F1.json",
            "voice_styles/F2.json",
        ];

        for file in files {
            let dest = supertonic_dir.join(file);

            // Create parent directory if needed
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create directory {}", parent.display()))?;
            }

            self.download_file(repo_id, revision, file, &dest)?;
        }

        info!("✓ Supertonic model downloaded successfully");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_type_from_str() {
        assert_eq!(
            ModelType::from_str("sensevoice"),
            Some(ModelType::Sensevoice)
        );
        assert_eq!(
            ModelType::from_str("SUPERTONIC"),
            Some(ModelType::Supertonic)
        );
        assert_eq!(ModelType::from_str("all"), Some(ModelType::All));
        assert_eq!(ModelType::from_str("invalid"), None);
    }
}
