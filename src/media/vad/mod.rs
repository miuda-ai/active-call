use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::media::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::any::Any;
use tokio_util::sync::CancellationToken;

pub(crate) mod simd;
pub mod tiny_silero;
pub(crate) mod utils;
pub use tiny_silero::TinySilero;

#[cfg(test)]
mod benchmark_all;
#[cfg(test)]
mod tests;

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct VADOption {
    pub r#type: VadType,
    pub samplerate: u32,
    /// Padding before speech detection (in ms)
    pub speech_padding: u64,
    /// Padding after silence detection (in ms)
    pub silence_padding: u64,
    pub ratio: f32,
    pub voice_threshold: f32,
    pub max_buffer_duration_secs: u64,
    /// Timeout duration for silence (in ms), None means disable this feature
    pub silence_timeout: Option<u64>,
    pub endpoint: Option<String>,
    pub secret_key: Option<String>,
    pub secret_id: Option<String>,
}

impl Default for VADOption {
    fn default() -> Self {
        Self {
            r#type: VadType::Silero,
            samplerate: 16000,
            speech_padding: 200,  // min_speech_duration_ms
            silence_padding: 100, // min_silence_duration_ms
            ratio: 0.5,
            voice_threshold: 0.5,
            max_buffer_duration_secs: 50,
            silence_timeout: None,
            endpoint: None,
            secret_key: None,
            secret_id: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum VadType {
    Silero,
    Other(String),
}

impl<'de> Deserialize<'de> for VadType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "silero" => Ok(VadType::Silero),
            _ => Ok(VadType::Other(value)),
        }
    }
}

impl std::fmt::Display for VadType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VadType::Silero => write!(f, "silero"),
            VadType::Other(provider) => write!(f, "{}", provider),
        }
    }
}

impl TryFrom<&String> for VadType {
    type Error = String;

    fn try_from(value: &String) -> std::result::Result<Self, Self::Error> {
        match value.as_str() {
            "silero" => Ok(VadType::Silero),
            other => Ok(VadType::Other(other.to_string())),
        }
    }
}
struct SpeechBuf {
    samples: PcmBuf,
    timestamp: u64,
}

struct VadProcessorInner {
    vad: Box<dyn VadEngine>,
    event_sender: EventSender,
    option: VADOption,
    window_bufs: Vec<SpeechBuf>,
    triggered: bool,
    current_speech_start: Option<u64>,
    temp_end: Option<u64>,
}
pub struct VadProcessor {
    inner: VadProcessorInner,
}

pub trait VadEngine: Send + Sync + Any {
    fn process(&mut self, frame: &mut AudioFrame) -> Vec<(bool, u64)>;
}

impl VadProcessorInner {
    pub fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };

        let samples_cloned = samples.to_owned();
        let results = self.vad.process(frame);
        for (is_speaking, timestamp) in results {
            if is_speaking || self.triggered {
                let current_buf = SpeechBuf {
                    samples: samples_cloned.clone(),
                    timestamp,
                };
                self.window_bufs.push(current_buf);
            }
            self.process_vad_logic(is_speaking, timestamp, &frame.track_id)?;

            // Clean up old buffers periodically
            if self.window_bufs.len() > 1000 || !self.triggered {
                let cutoff = if self.triggered {
                    timestamp.saturating_sub(5000)
                } else {
                    timestamp.saturating_sub(self.option.silence_padding)
                };
                self.window_bufs.retain(|buf| buf.timestamp > cutoff);
            }
        }

        Ok(())
    }

    fn process_vad_logic(
        &mut self,
        is_speaking: bool,
        timestamp: u64,
        track_id: &str,
    ) -> Result<()> {
        if is_speaking && !self.triggered {
            self.triggered = true;
            self.current_speech_start = Some(timestamp);
            let event = SessionEvent::Speaking {
                track_id: track_id.to_string(),
                timestamp: crate::media::get_timestamp(),
                start_time: timestamp,
            };
            self.event_sender.send(event).ok();
        } else if !is_speaking {
            if self.temp_end.is_none() {
                self.temp_end = Some(timestamp);
            }

            if let Some(temp_end) = self.temp_end {
                // Use saturating_sub to handle timestamp wrapping or out-of-order frames
                let silence_duration = timestamp.saturating_sub(temp_end);

                // Process regular silence detection for speech segments
                if self.triggered && silence_duration >= self.option.silence_padding {
                    if let Some(start_time) = self.current_speech_start {
                        // Use safe duration calculation
                        let duration = temp_end.saturating_sub(start_time);
                        if duration >= self.option.speech_padding {
                            let samples_vec = self
                                .window_bufs
                                .iter()
                                .filter(|buf| {
                                    buf.timestamp >= start_time && buf.timestamp <= temp_end
                                })
                                .flat_map(|buf| buf.samples.iter())
                                .cloned()
                                .collect();
                            self.window_bufs.clear();

                            let event = SessionEvent::Silence {
                                track_id: track_id.to_string(),
                                timestamp: crate::media::get_timestamp(),
                                start_time,
                                duration,
                                samples: Some(samples_vec),
                            };
                            self.event_sender.send(event).ok();
                        }
                    }
                    self.triggered = false;
                    self.current_speech_start = None;
                    self.temp_end = Some(timestamp); // Update temp_end for silence timeout tracking
                }

                // Process silence timeout if configured
                if let Some(timeout) = self.option.silence_timeout {
                    // Use same safe calculation for silence timeout
                    let timeout_duration = timestamp.saturating_sub(temp_end);

                    if timeout_duration >= timeout {
                        let event = SessionEvent::Silence {
                            track_id: track_id.to_string(),
                            timestamp: crate::media::get_timestamp(),
                            start_time: temp_end,
                            duration: timeout_duration,
                            samples: None,
                        };
                        self.event_sender.send(event).ok();
                        self.temp_end = Some(timestamp);
                    }
                }
            }
        } else if is_speaking && self.temp_end.is_some() {
            self.temp_end = None;
        }

        Ok(())
    }
}

impl VadProcessor {
    pub fn create(
        _token: CancellationToken,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Box<dyn Processor>> {
        let vad: Box<dyn VadEngine> = match option.r#type {
            VadType::Silero => Box::new(tiny_silero::TinySilero::new(option.clone())?),
            _ => Box::new(NopVad::new()?),
        };
        Ok(Box::new(VadProcessor::new(vad, event_sender, option)?))
    }

    pub fn create_nop(
        _token: CancellationToken,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Box<dyn Processor>> {
        let vad: Box<dyn VadEngine> = match option.r#type {
            _ => Box::new(NopVad::new()?),
        };
        Ok(Box::new(VadProcessor::new(vad, event_sender, option)?))
    }

    pub fn new(
        engine: Box<dyn VadEngine>,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Self> {
        let inner = VadProcessorInner {
            vad: engine,
            event_sender,
            option,
            window_bufs: Vec::new(),
            triggered: false,
            current_speech_start: None,
            temp_end: None,
        };
        Ok(Self { inner })
    }
}

impl Processor for VadProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        self.inner.process_frame(frame)
    }
}

struct NopVad {}

impl NopVad {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

impl VadEngine for NopVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Vec<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return vec![(false, frame.timestamp)],
        };
        // Check if there are any non-zero samples
        let has_speech = samples.iter().any(|&x| x != 0);
        vec![(has_speech, frame.timestamp)]
    }
}
