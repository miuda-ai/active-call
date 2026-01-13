use super::INTERNAL_SAMPLERATE;
use super::track::track_codec::TrackCodec;
use crate::media::{AudioFrame, Samples};
use anyhow::Result;
use std::any::Any;
use std::sync::{Arc, Mutex};

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
}

impl ProcessorChain {
    pub fn new(_sample_rate: u32) -> Self {
        Self {
            processors: Arc::new(Mutex::new(Vec::new())),
            codec: TrackCodec::new(),
            sample_rate: INTERNAL_SAMPLERATE,
            force_decode: true,
        }
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
        if !self.force_decode && processors.is_empty() {
            return Ok(());
        }

        match &mut frame.samples {
            Samples::RTP {
                payload_type,
                payload,
                ..
            } => {
                if TrackCodec::is_audio(*payload_type) {
                    let (decoded_sample_rate, channels, samples) =
                        self.codec.decode(*payload_type, &payload, self.sample_rate);
                    frame.channels = channels;
                    frame.samples = Samples::PCM { samples };
                    frame.sample_rate = decoded_sample_rate;
                }
            }
            _ => {}
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
