use super::model::{Style, SupertonicModel, load_voice_style};
use super::processor::chunk_text;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct SupertonicTts {
    model: SupertonicModel,
    voice_styles_dir: PathBuf,
    style_cache: HashMap<String, Style>,
}

impl SupertonicTts {
    pub fn new<P: AsRef<Path>>(
        onnx_dir: P,
        config_path: P,
        voice_styles_dir: P,
        threads: usize,
        _use_gpu: bool,
    ) -> Result<Self> {
        let model = SupertonicModel::new(&onnx_dir, &config_path, &voice_styles_dir, threads)?;

        Ok(Self {
            model,
            voice_styles_dir: voice_styles_dir.as_ref().to_path_buf(),
            style_cache: HashMap::new(),
        })
    }

    pub fn sample_rate(&self) -> i32 {
        self.model.sample_rate
    }

    pub fn synthesize(
        &mut self,
        text: &str,
        language: &str,
        voice_style: Option<&str>,
        speed: Option<f32>,
    ) -> Result<Vec<f32>> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Vec::new());
        }

        let speed = speed.unwrap_or(1.0);
        let style_name = voice_style.unwrap_or("M1");

        // Ensure style is loaded
        if !self.style_cache.contains_key(style_name) {
            let path = self.voice_styles_dir.join(format!("{}.json", style_name));
            if !path.exists() {
                return Err(anyhow!("Voice style file not found: {}", path.display()));
            }
            let style = load_voice_style(&[path.to_string_lossy().to_string()])?;
            self.style_cache.insert(style_name.to_string(), style);
        }

        let style = self.style_cache.get(style_name).unwrap();

        // Chunk text
        let chunks = chunk_text(text, None);
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let langs = vec![language.to_string(); chunks.len()];

        let mut final_audio = Vec::new();

        for (chunk, lang) in chunks.iter().zip(langs.iter()) {
            let (audios, _) = self.model.infer(
                &[chunk.clone()],
                &[lang.clone()],
                style,
                5, // default steps
                speed,
            )?;

            if let Some(audio) = audios.into_iter().next() {
                final_audio.extend(audio);
            }
        }

        Ok(final_audio)
    }
}
