use super::processor::Processor;
use crate::media::{AudioFrame, INTERNAL_SAMPLERATE, Samples};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AmbianceOption {
    pub path: Option<String>,
    pub duck_level: Option<f32>,
    pub normal_level: Option<f32>,
    pub transition_speed: Option<f32>,
    pub enabled: Option<bool>,
}

impl AmbianceOption {
    pub fn merge(&mut self, other: &AmbianceOption) {
        if self.path.is_none() {
            self.path = other.path.clone();
        }
        if self.duck_level.is_none() {
            self.duck_level = other.duck_level;
        }
        if self.normal_level.is_none() {
            self.normal_level = other.normal_level;
        }
        if self.transition_speed.is_none() {
            self.transition_speed = other.transition_speed;
        }
        if self.enabled.is_none() {
            self.enabled = other.enabled;
        }
    }
}

pub struct AmbianceProcessor {
    samples: Vec<i16>,
    cursor: usize,
    duck_level: f32,
    normal_level: f32,
    enabled: bool,
    current_level: f32,
    transition_speed: f32,
}

impl AmbianceProcessor {
    pub async fn new(option: AmbianceOption) -> Result<Self> {
        let path = option
            .path
            .ok_or_else(|| anyhow::anyhow!("Ambiance path required"))?;

        let samples =
            crate::media::loader::load_audio_as_pcm(&path, INTERNAL_SAMPLERATE, true).await?;

        info!("Loading ambiance {}: samples={}", path, samples.len());

        let normal_level = option.normal_level.unwrap_or(0.8);
        Ok(Self {
            samples,
            cursor: 0,
            duck_level: option.duck_level.unwrap_or(0.2), // Duck to 20% volume
            normal_level,                                 // Play at 80% volume normally
            enabled: option.enabled.unwrap_or(true),
            current_level: normal_level,
            transition_speed: option.transition_speed.unwrap_or(0.05), // Smooth transition per frame
        })
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_levels(&mut self, normal: f32, duck: f32) {
        self.normal_level = normal;
        self.duck_level = duck;
    }

    fn get_next_ambient_sample(&mut self) -> i16 {
        if self.samples.is_empty() {
            return 0;
        }
        let s = self.samples[self.cursor];
        self.cursor = (self.cursor + 1) % self.samples.len();
        s
    }
}

impl Processor for AmbianceProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if !self.enabled || self.samples.is_empty() {
            return Ok(());
        }

        let is_server_side_speaking = match &frame.samples {
            Samples::PCM { samples } => !samples.is_empty(),
            Samples::RTP { .. } => true,
            Samples::Empty => false,
        };

        let target_level = if is_server_side_speaking {
            self.duck_level
        } else {
            self.normal_level
        };

        if (self.current_level - target_level).abs() > 0.001 {
            if self.current_level < target_level {
                self.current_level = (self.current_level + self.transition_speed).min(target_level);
            } else {
                self.current_level = (self.current_level - self.transition_speed).max(target_level);
            }
        }

        match &mut frame.samples {
            Samples::PCM { samples } => {
                for s in samples.iter_mut() {
                    let ambient = self.get_next_ambient_sample() as f32 * self.current_level;
                    let mixed = *s as f32 + ambient;
                    *s = mixed.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                }
            }
            Samples::Empty => {
                let sample_rate = if frame.sample_rate > 0 {
                    frame.sample_rate
                } else {
                    INTERNAL_SAMPLERATE
                };
                let frame_size = (sample_rate as usize * 20) / 1000;
                let mut ambient_samples = Vec::with_capacity(frame_size);
                for _ in 0..frame_size {
                    let ambient = self.get_next_ambient_sample() as f32 * self.current_level;
                    ambient_samples.push(ambient.clamp(i16::MIN as f32, i16::MAX as f32) as i16);
                }
                frame.samples = Samples::PCM {
                    samples: ambient_samples,
                };
                frame.sample_rate = sample_rate;
                frame.channels = 1;
            }
            _ => {}
        }

        Ok(())
    }
}
