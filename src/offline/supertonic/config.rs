use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ae: AEConfig,
    #[serde(default)]
    pub ttl: TTLConfig,

    // Fallback fields for flat config structure (e.g. from config.json directly)
    #[serde(default, alias = "sampling_rate")]
    pub sample_rate: i32,
    #[serde(default)]
    pub base_chunk_size: i32,
    #[serde(default)]
    pub chunk_compress_factor: i32,
    #[serde(default)]
    pub latent_dim: i32,
}

impl Config {
    // Helper to consolidate fields after loading
    pub fn fix(&mut self) {
        if self.ae.sample_rate == 0 && self.sample_rate > 0 {
            self.ae.sample_rate = self.sample_rate;
        }
        if self.ae.base_chunk_size == 0 && self.base_chunk_size > 0 {
            self.ae.base_chunk_size = self.base_chunk_size;
        }
        if self.ttl.chunk_compress_factor == 0 && self.chunk_compress_factor > 0 {
            self.ttl.chunk_compress_factor = self.chunk_compress_factor;
        }
        if self.ttl.latent_dim == 0 && self.latent_dim > 0 {
            self.ttl.latent_dim = self.latent_dim;
        }

        // Defaults if still missing
        if self.ae.sample_rate == 0 {
            self.ae.sample_rate = 24000;
        } // or 44100
        // Wait, downloaded config says 44100!
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AEConfig {
    #[serde(default)]
    pub sample_rate: i32,
    #[serde(default)]
    pub base_chunk_size: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TTLConfig {
    #[serde(default)]
    pub chunk_compress_factor: i32,
    #[serde(default)]
    pub latent_dim: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceStyleData {
    pub style_ttl: StyleComponent,
    pub style_dp: StyleComponent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleComponent {
    pub data: Vec<Vec<Vec<f32>>>,
    pub dims: Vec<usize>,
    #[serde(rename = "type")]
    pub dtype: String,
}
