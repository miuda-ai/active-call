use super::processor::Processor;
use crate::media::{AudioFrame, INTERNAL_SAMPLERATE, Samples};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
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
    resample_phase: u32,
    resample_step: u32,
}

impl AmbianceProcessor {
    pub async fn new(option: AmbianceOption) -> Result<Self> {
        let path = option
            .path
            .ok_or_else(|| anyhow::anyhow!("Ambiance path required"))?;

        let samples =
            crate::media::loader::load_audio_as_pcm(&path, INTERNAL_SAMPLERATE, true).await?;

        info!("Loading ambiance {}: samples={}", path, samples.len());

        let normal_level = option.normal_level.unwrap_or(0.3);
        Ok(Self {
            samples,
            cursor: 0,
            duck_level: option.duck_level.unwrap_or(0.1),
            normal_level,
            enabled: option.enabled.unwrap_or(true),
            current_level: normal_level,
            transition_speed: option.transition_speed.unwrap_or(0.01),
            resample_phase: 0,
            resample_step: 1 << 16,
        })
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_levels(&mut self, normal: f32, duck: f32) {
        self.normal_level = normal;
        self.duck_level = duck;
    }

    #[inline]
    fn get_ambient_sample_with_rate(&mut self, target_sample_rate: u32) -> i16 {
        if self.samples.is_empty() {
            return 0;
        }

        self.resample_step =
            (((INTERNAL_SAMPLERATE as u64) << 16) / target_sample_rate as u64) as u32;
        let sample = self.samples[self.cursor];

        self.resample_phase += self.resample_step;
        while self.resample_phase >= (1 << 16) {
            self.resample_phase -= 1 << 16;
            self.cursor = (self.cursor + 1) % self.samples.len();
        }

        sample
    }

    #[inline]
    fn soft_mix(signal: i16, ambient: i16, level: f32) -> i16 {
        let ambient_scaled = (ambient as i32 * (level * 256.0) as i32) >> 8;
        let signal_i32 = signal as i32;
        let mixed = signal_i32 + ambient_scaled;

        if mixed > 32767 {
            let over = mixed - 32767;
            (32767 - (over >> 2)) as i16
        } else if mixed < -32768 {
            let under = -32768 - mixed;
            (-32768 + (under >> 2)) as i16
        } else {
            mixed as i16
        }
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

        let sample_rate = if frame.sample_rate > 0 {
            frame.sample_rate
        } else {
            INTERNAL_SAMPLERATE
        };
        let channels = frame.channels.max(1) as usize;

        match &mut frame.samples {
            Samples::PCM { samples } => {
                let frame_sample_count = samples.len() / channels;
                for i in 0..frame_sample_count {
                    let ambient = self.get_ambient_sample_with_rate(sample_rate);
                    for c in 0..channels {
                        let idx = i * channels + c;
                        if idx < samples.len() {
                            samples[idx] =
                                Self::soft_mix(samples[idx], ambient, self.current_level);
                        }
                    }
                }
            }
            Samples::Empty => {
                let frame_size = (sample_rate as usize * 20) / 1000;
                let mut ambient_samples = Vec::with_capacity(frame_size * channels);
                for _ in 0..frame_size {
                    let ambient = self.get_ambient_sample_with_rate(sample_rate);
                    let ambient_scaled =
                        ((ambient as i32 * (self.current_level * 256.0) as i32) >> 8) as i16;
                    for _ in 0..channels {
                        ambient_samples.push(ambient_scaled);
                    }
                }
                frame.samples = Samples::PCM {
                    samples: ambient_samples,
                };
                frame.sample_rate = sample_rate;
                frame.channels = channels as u16;
            }
            _ => {}
        }

        Ok(())
    }
}

/// Share one loaded wav / playhead across the TTS mixer and the idle filler.
#[derive(Clone)]
pub struct SharedAmbianceProcessor {
    inner: Arc<Mutex<AmbianceProcessor>>,
}

impl SharedAmbianceProcessor {
    pub fn new(inner: Arc<Mutex<AmbianceProcessor>>) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> Arc<Mutex<AmbianceProcessor>> {
        self.inner.clone()
    }
}

impl Processor for SharedAmbianceProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        self.inner.lock().unwrap().process_frame(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::AudioFrame;

    fn loud_processor() -> AmbianceProcessor {
        AmbianceProcessor {
            samples: vec![8000i16; INTERNAL_SAMPLERATE as usize],
            cursor: 0,
            duck_level: 0.5,
            normal_level: 1.0,
            enabled: true,
            current_level: 1.0,
            transition_speed: 1.0,
            resample_phase: 0,
            resample_step: 1 << 16,
        }
    }

    #[test]
    fn empty_frame_becomes_ambiance_pcm() {
        let mut processor = loud_processor();
        let mut frame = AudioFrame {
            track_id: "server-side-track".to_string(),
            samples: Samples::Empty,
            timestamp: 0,
            sample_rate: INTERNAL_SAMPLERATE,
            channels: 1,
            ..Default::default()
        };
        processor.process_frame(&mut frame).unwrap();
        match frame.samples {
            Samples::PCM { samples } => {
                assert_eq!(samples.len(), 320);
                assert!(
                    samples.iter().any(|s| *s != 0),
                    "idle frame should carry ambiance"
                );
            }
            other => panic!("expected PCM, got {:?}", other),
        }
    }

    #[test]
    fn pcm_frame_is_mixed() {
        let mut processor = loud_processor();
        let mut frame = AudioFrame {
            track_id: "server-side-track".to_string(),
            samples: Samples::PCM {
                samples: vec![1000; 320],
            },
            timestamp: 0,
            sample_rate: INTERNAL_SAMPLERATE,
            channels: 1,
            ..Default::default()
        };
        processor.process_frame(&mut frame).unwrap();
        match frame.samples {
            Samples::PCM { samples } => {
                assert!(
                    samples.iter().any(|s| *s != 1000),
                    "tts frame should mix ambiance"
                );
            }
            other => panic!("expected PCM, got {:?}", other),
        }
    }

    /// Cost of one idle tick (20ms frame mix) — the idle loop runs this 50x/s
    /// per call. Skipped in debug builds (repo convention, see perf_analysis.rs).
    #[test]
    fn perf_idle_mix_cost() {
        if cfg!(debug_assertions) {
            println!("Skipping ambiance idle mix perf test in debug mode.");
            return;
        }
        let mut processor = loud_processor();
        let budget_us = 100.0;
        let iterations = 10_000u32;

        let start = std::time::Instant::now();
        for i in 0..iterations {
            let mut frame = AudioFrame {
                track_id: "server-side-track".to_string(),
                samples: Samples::Empty,
                timestamp: i as u64,
                sample_rate: INTERNAL_SAMPLERATE,
                channels: 1,
                ..Default::default()
            };
            processor.process_frame(&mut frame).unwrap();
        }
        let per_call_us = start.elapsed().as_micros() as f64 / iterations as f64;
        println!("ambiance idle mix: {:.2} µs per 20ms frame", per_call_us);
        assert!(
            per_call_us < budget_us,
            "idle mix {:.2} µs/frame exceeds {:.0} µs budget",
            per_call_us,
            budget_us
        );
    }
}
