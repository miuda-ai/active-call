//! Ringback / line-state detection.
//!
//! Feature gating lives on the `mod` declaration in `media::mod`, so this
//! subtree is only compiled when `ringback-detection` is enabled.

pub mod processor;

mod model;
mod model_cache;
mod pool;
mod ring;
mod state;

#[cfg(test)]
pub(crate) mod test_support {
    use super::model_cache::{self, RingbackModel};
    use super::processor::{MODEL_NUM_SAMPLES, SAMPLE_RATE};
    use super::ring::AudioRing;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// The weights file is not checked in (`*.bin` is git-ignored), so tests
    /// that need the real model skip when it is absent. Set
    /// `TELCOCLASSIFIER_WEIGHTS` to point at it explicitly.
    pub(crate) fn weights_path() -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if let Ok(path) = std::env::var("TELCOCLASSIFIER_WEIGHTS") {
            candidates.push(PathBuf::from(path));
        }
        candidates.push(PathBuf::from("./telcoclassifier_weights.bin"));
        candidates.push(PathBuf::from("../telcoclassifier_weights.bin"));
        candidates.into_iter().find(|path| path.is_file())
    }

    pub(crate) fn model() -> Option<Arc<RingbackModel>> {
        model_cache::load(weights_path()?).ok()
    }

    /// Chinese ringback tone: 450 Hz, 1 s on / 4 s off.
    pub(crate) fn pcm_cn_ringback(seconds: f32) -> Vec<i16> {
        tone(&[450.0], 1000, 4000, seconds)
    }

    /// Chinese busy tone: 450 Hz, 350 ms on / 350 ms off.
    pub(crate) fn pcm_cn_busy(seconds: f32) -> Vec<i16> {
        tone(&[450.0], 350, 350, seconds)
    }

    fn tone(freqs: &[f32], on_ms: u32, off_ms: u32, seconds: f32) -> Vec<i16> {
        let sample_rate = SAMPLE_RATE as f32;
        let total = (seconds * sample_rate) as usize;
        let period = (on_ms + off_ms) as f32 * sample_rate / 1000.0;
        let on = on_ms as f32 * sample_rate / 1000.0;
        let mut out = vec![0i16; total];
        for (i, sample) in out.iter_mut().enumerate() {
            let phase = (i as f32) % period;
            if phase < on {
                let mut value = 0.0f32;
                for freq in freqs {
                    value += (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate).sin();
                }
                value = value / freqs.len() as f32 * 0.3;
                *sample = (value * 32767.0) as i16;
            }
        }
        out
    }

    /// Fills the model window from `pcm` exactly like the processor would.
    pub(crate) fn window(pcm: &[i16], out: &mut Vec<f32>) {
        let mut ring = AudioRing::new(MODEL_NUM_SAMPLES);
        ring.push_i16(pcm);
        ring.snapshot(out);
    }

    pub(crate) fn load_wav(path: &str) -> Option<Vec<i16>> {
        let mut reader = hound::WavReader::open(path).ok()?;
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, SAMPLE_RATE, "{path}");
        assert_eq!(spec.channels, 1, "{path}");
        assert_eq!(spec.bits_per_sample, 16, "{path}");
        Some(
            reader
                .samples::<i16>()
                .map(|sample| sample.expect("pcm sample"))
                .collect(),
        )
    }
}
