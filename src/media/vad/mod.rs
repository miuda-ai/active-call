use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::media::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::any::Any;
use std::collections::VecDeque;
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
    /// Audio quieter than this (RMS, dBFS) is never speech, whatever the model
    /// says; None disables. Faint background talk - the call-centre ambience
    /// some voice assistants play under themselves - otherwise reads as speech
    /// once heard for a few seconds.
    pub min_level_dbfs: Option<f32>,
    pub max_buffer_duration_secs: u64,
    /// Timeout duration for silence (in ms), None means disable this feature
    pub silence_timeout: Option<u64>,
    pub endpoint: Option<String>,
    pub secret_key: Option<String>,
    pub secret_id: Option<String>,
    #[serde(skip)]
    pub refer: Option<bool>,
}

impl Default for VADOption {
    fn default() -> Self {
        Self {
            r#type: VadType::Silero,
            samplerate: 16000,
            speech_padding: 250, // min_speech_duration_ms (match silero_vad default)
            silence_padding: 100, // min_silence_duration_ms
            ratio: 0.5,
            voice_threshold: 0.5,
            min_level_dbfs: None,
            max_buffer_duration_secs: 50,
            silence_timeout: None,
            endpoint: None,
            secret_key: None,
            secret_id: None,
            refer: None,
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
/// Audio kept while idle, so an utterance can start a little before the
/// frame that completed its first speech chunk (a chunk spans earlier frames).
const IDLE_KEEP_MS: u64 = 500;

/// Recent input, contiguous, so an utterance's samples can be cut out by
/// timestamp. Engines report one decision per chunk of their own size (512
/// samples for silero), which doesn't line up with the frames (e.g. 320
/// samples at 20 ms ptime) - so audio is kept per sample, not per decision.
struct SampleBuf {
    samples: VecDeque<i16>,
    /// absolute index (samples since the first frame) of `samples[0]`
    start: u64,
    /// timestamp of the first frame, which engine timestamps count from
    base_ts: Option<u64>,
    sample_rate: u64,
}

impl SampleBuf {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            start: 0,
            base_ts: None,
            sample_rate: 16000,
        }
    }

    fn push(&mut self, frame_ts: u64, sample_rate: u32, samples: &[i16]) {
        self.base_ts.get_or_insert(frame_ts);
        if sample_rate > 0 {
            self.sample_rate = sample_rate as u64;
        }
        self.samples.extend(samples);
    }

    /// Absolute sample index of an engine timestamp.
    fn index(&self, ts: u64) -> u64 {
        ts.saturating_sub(self.base_ts.unwrap_or(ts)) * self.sample_rate / 1000
    }

    /// Samples in [from_ts, to_ts), clamped to what is still buffered.
    fn slice(&self, from_ts: u64, to_ts: u64) -> PcmBuf {
        let end = self.start + self.samples.len() as u64;
        let from = self.index(from_ts).clamp(self.start, end);
        let to = self.index(to_ts).clamp(from, end);
        self.samples
            .range((from - self.start) as usize..(to - self.start) as usize)
            .copied()
            .collect()
    }

    /// Drops everything before `ts`.
    fn trim_before(&mut self, ts: u64) {
        let drop = self
            .index(ts)
            .saturating_sub(self.start)
            .min(self.samples.len() as u64);
        self.samples.drain(..drop as usize);
        self.start += drop;
    }

    /// Drops all but the last `ms`.
    fn keep_last(&mut self, ms: u64) {
        let keep = (ms * self.sample_rate / 1000) as usize;
        let drop = self.samples.len().saturating_sub(keep);
        self.samples.drain(..drop);
        self.start += drop as u64;
    }
}

struct VadProcessorInner {
    vad: Box<dyn VadEngine>,
    event_sender: EventSender,
    option: VADOption,
    buf: SampleBuf,
    triggered: bool,
    triggered_event_sent: bool,
    current_speech_start: Option<u64>,
    temp_end: Option<u64>,
    refer: Option<bool>,
}
pub struct VadProcessor {
    inner: VadProcessorInner,
}

pub trait VadEngine: Send + Sync + Any {
    fn process(&mut self, frame: &mut AudioFrame) -> Vec<(bool, u64)>;

    /// Latest raw speech probability in [0, 1] from the underlying model.
    /// Returns None for engines that don't produce a probability (e.g. NopVad).
    fn last_probability(&self) -> Option<f32> {
        None
    }
}

impl VadProcessorInner {
    pub fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };

        // every frame is kept, not just those completing an engine chunk:
        // decisions are cut back out of this by timestamp
        self.buf.push(frame.timestamp, frame.sample_rate, samples);
        let results = self.vad.process(frame);
        frame.speech_probability = self.vad.last_probability();
        for (is_speaking, timestamp) in results {
            self.process_vad_logic(is_speaking, timestamp, &frame.track_id)?;
        }

        if self.triggered {
            let max_ms = self.option.max_buffer_duration_secs.max(1) * 1000;
            if self.buf.samples.len() as u64 > max_ms * self.buf.sample_rate / 1000 {
                // an utterance this long loses its beginning, not the whole buffer
                self.buf.keep_last(max_ms);
            }
        } else {
            self.buf.keep_last(IDLE_KEEP_MS);
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
            self.triggered_event_sent = false;
        } else if is_speaking && self.triggered {
            // Already triggered, check if we need to emit Speaking event for the first time
            if let Some(start_time) = self.current_speech_start {
                let duration = timestamp.saturating_sub(start_time);
                // L1 filter: only emit Speaking if duration is enough (e.g. 200ms)
                // Use speech_padding as min_speech_duration
                if duration >= self.option.speech_padding && !self.triggered_event_sent {
                    let event = SessionEvent::Speaking {
                        track_id: track_id.to_string(),
                        timestamp: crate::media::get_timestamp(),
                        start_time,
                        is_filler: None, // Will be enriched by MFCC or ASR
                        confidence: Some(1.0),
                        refer: self.refer,
                    };
                    self.event_sender.send(event).ok();
                    self.triggered_event_sent = true;
                }
            }
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
                            // temp_end is the first non-speech chunk's start
                            let samples_vec = self.buf.slice(start_time, temp_end);
                            self.buf.trim_before(temp_end);

                            let event = SessionEvent::Silence {
                                track_id: track_id.to_string(),
                                timestamp: crate::media::get_timestamp(),
                                start_time,
                                duration,
                                samples: Some(samples_vec),
                                refer: self.refer,
                            };
                            self.event_sender.send(event).ok();
                        }
                    }
                    self.triggered = false;
                    self.triggered_event_sent = false;
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
                            refer: self.refer,
                        };
                        self.event_sender.send(event).ok();
                        self.temp_end = Some(timestamp);
                    }
                }
            }
        }

        if is_speaking && self.temp_end.is_some() {
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
        let refer = option.refer;
        let inner = VadProcessorInner {
            vad: engine,
            event_sender,
            option,
            buf: SampleBuf::new(),
            triggered_event_sent: false,
            triggered: false,
            current_speech_start: None,
            temp_end: None,
            refer,
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
