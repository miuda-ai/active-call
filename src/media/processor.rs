use super::INTERNAL_SAMPLERATE;
use super::track::track_codec::TrackCodec;
use crate::event::{EventSender, SessionEvent};
use crate::media::{AudioFrame, Samples, SourcePacket};
use anyhow::Result;
use std::any::Any;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;

pub trait Processor: Send + Sync + Any {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()>;
}

pub fn convert_to_mono(samples: &mut Vec<i16>, channels: u16) {
    if channels != 2 {
        return;
    }
    let mut i = 0;
    let mut j = 0;
    while i < samples.len() {
        let l = samples[i] as i32;
        let r = samples[i + 1] as i32;
        samples[j] = ((l + r) / 2) as i16;
        i += 2;
        j += 1;
    }
    samples.truncate(j);
}

impl Default for AudioFrame {
    fn default() -> Self {
        Self {
            track_id: "".to_string(),
            samples: Samples::Empty,
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
            src_packet: None,
            speech_probability: None,
        }
    }
}

impl Samples {
    pub fn is_empty(&self) -> bool {
        match self {
            Samples::PCM { samples } => samples.is_empty(),
            Samples::RTP { payload, .. } => payload.is_empty(),
            Samples::Empty => true,
        }
    }
}

#[derive(Clone)]
pub struct ProcessorChain {
    processors: Arc<Mutex<Vec<Box<dyn Processor>>>>,
    pub codec: TrackCodec,
    sample_rate: u32,
    pub force_decode: bool,
    /// Optional raw tap: when set, frames are mirrored to this channel at
    /// their native (pre-resample) sample rate right after decoding, before
    /// the pipeline normalizes them to `INTERNAL_SAMPLERATE`. Used by the
    /// native-samplerate recorder.
    ///
    /// Shared via `Arc<RwLock<..>>`: `RtcTrack::create()` clones the chain
    /// into its long-lived worker tasks *before* the recorder attaches the
    /// tap, so a plain field would leave those workers with a stale `None`.
    pub raw_tap: Arc<RwLock<Option<mpsc::UnboundedSender<AudioFrame>>>>,
}

impl ProcessorChain {
    pub fn new(_sample_rate: u32) -> Self {
        Self {
            processors: Arc::new(Mutex::new(Vec::new())),
            codec: TrackCodec::new(),
            sample_rate: INTERNAL_SAMPLERATE,
            force_decode: true,
            raw_tap: Arc::new(RwLock::new(None)),
        }
    }
    pub fn set_raw_tap(&mut self, tap: Option<mpsc::UnboundedSender<AudioFrame>>) {
        *self.raw_tap.write().unwrap() = tap;
    }
    fn raw_tap(&self) -> Option<mpsc::UnboundedSender<AudioFrame>> {
        self.raw_tap.read().unwrap().clone()
    }
    pub fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processors.lock().unwrap().insert(0, processor);
    }
    pub fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processors.lock().unwrap().push(processor);
    }

    pub fn has_processor<T: 'static>(&self) -> bool {
        let processors = self.processors.lock().unwrap();
        processors
            .iter()
            .any(|processor| (processor.as_ref() as &dyn Any).is::<T>())
    }

    pub fn remove_processor<T: 'static>(&self) {
        let mut processors = self.processors.lock().unwrap();
        processors.retain(|processor| !(processor.as_ref() as &dyn Any).is::<T>());
    }

    pub fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        let mut processors = self.processors.lock().unwrap();
        if !self.force_decode && processors.is_empty() && self.raw_tap().is_none() {
            return Ok(());
        }
        match &mut frame.samples {
            Samples::RTP {
                payload_type,
                payload,
                sequence_number,
            } => {
                if TrackCodec::is_audio(*payload_type) {
                    let (decoded_sample_rate, channels, samples) =
                        self.codec.decode(*payload_type, &payload);
                    let src_packet = SourcePacket {
                        sequence_number: *sequence_number,
                        payload_type: *payload_type,
                        payload: std::mem::take(payload),
                    };
                    frame.src_packet = Some(src_packet);
                    frame.channels = channels;
                    frame.samples = Samples::PCM { samples };
                    frame.sample_rate = decoded_sample_rate;
                }
            }
            _ => {}
        }

        // Mirror the frame to the raw tap at its native sample rate, before
        // the pipeline resamples it to INTERNAL_SAMPLERATE.
        if let Some(tap) = self.raw_tap()
            && let Samples::PCM { samples } = &frame.samples
            && !samples.is_empty()
            && frame.sample_rate > 0
        {
            let mut raw = frame.clone();
            raw.src_packet = None;
            let mono = match &mut raw.samples {
                Samples::PCM { samples } => samples,
                _ => unreachable!("checked PCM above"),
            };
            if raw.channels == 2 {
                convert_to_mono(mono, 2);
                raw.channels = 1;
            }
            let _ = tap.send(raw);
        }

        if let Samples::PCM { samples } = &mut frame.samples {
            if frame.sample_rate != self.sample_rate {
                let new_samples = self.codec.resample(
                    std::mem::take(samples),
                    frame.sample_rate,
                    self.sample_rate,
                );
                *samples = new_samples;
                frame.sample_rate = self.sample_rate;
            }
            if frame.channels == 2 {
                convert_to_mono(samples, 2);
                frame.channels = 1;
            }
        }
        // Process the frame with all processors
        for processor in processors.iter_mut() {
            processor.process_frame(frame)?;
        }
        Ok(())
    }
}

pub struct SubscribeProcessor {
    event_sender: EventSender,
    track_id: String,
    track_index: u8, // 0 for caller, 1 for callee
}

impl SubscribeProcessor {
    pub fn new(event_sender: EventSender, track_id: String, track_index: u8) -> Self {
        Self {
            event_sender,
            track_id,
            track_index,
        }
    }
}

impl Processor for SubscribeProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if let Samples::PCM { samples } = &frame.samples {
            if !samples.is_empty() {
                let pcm_data = audio_codec::samples_to_bytes(samples);
                let mut data = Vec::with_capacity(pcm_data.len() + 1);
                data.push(self.track_index);
                data.extend_from_slice(&pcm_data);

                let event = SessionEvent::Binary {
                    track_id: self.track_id.clone(),
                    timestamp: frame.timestamp,
                    data,
                };
                self.event_sender.send(event).ok();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `RtcTrack::create()` clones the chain into its worker
    /// tasks *before* the recorder attaches the raw tap (native-samplerate
    /// recording). The tap must be shared state so clones taken before the
    /// attach observe it; with a plain `Option` field this test fails and
    /// SIP/RTP calls silently fall back to the 16 kHz recorder.
    #[test]
    fn raw_tap_visible_to_clones_taken_before_attach() {
        let mut chain = ProcessorChain::new(INTERNAL_SAMPLERATE);
        let mut worker = chain.clone();

        let (tx, mut rx) = mpsc::unbounded_channel();
        chain.set_raw_tap(Some(tx));

        let mut frame = AudioFrame {
            track_id: "track".to_string(),
            samples: Samples::PCM {
                samples: vec![100i16; 160],
            },
            sample_rate: 8000,
            channels: 1,
            ..Default::default()
        };
        worker.process_frame(&mut frame).unwrap();

        let raw = rx
            .try_recv()
            .expect("raw tap should receive a frame from a pre-attach clone");
        assert_eq!(raw.sample_rate, 8000);
        assert_eq!(raw.channels, 1);
        match raw.samples {
            Samples::PCM { samples } => assert_eq!(samples.len(), 160),
            _ => panic!("expected PCM samples on the raw tap"),
        }
        // The pipeline output is still normalized to the internal rate.
        assert_eq!(frame.sample_rate, INTERNAL_SAMPLERATE);
    }

    /// Detaching the tap on the original chain must be observed by clones as
    /// well (e.g. recorder restart swapping the sender).
    #[test]
    fn raw_tap_detach_visible_to_clones() {
        let mut chain = ProcessorChain::new(INTERNAL_SAMPLERATE);
        let (tx, _rx) = mpsc::unbounded_channel();
        chain.set_raw_tap(Some(tx));
        let mut worker = chain.clone();
        chain.set_raw_tap(None);

        let mut frame = AudioFrame {
            track_id: "track".to_string(),
            samples: Samples::PCM {
                samples: vec![100i16; 160],
            },
            sample_rate: 8000,
            channels: 1,
            ..Default::default()
        };
        worker.process_frame(&mut frame).unwrap();
        assert_eq!(frame.sample_rate, INTERNAL_SAMPLERATE);
    }
}
