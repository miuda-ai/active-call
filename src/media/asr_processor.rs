use super::processor::Processor;
use crate::{media::AudioFrame, media::Samples, transcription::TranscriptionClient};
use anyhow::Result;

pub struct AsrProcessor {
    pub asr_client: Box<dyn TranscriptionClient>,
}

impl AsrProcessor {}

impl Processor for AsrProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        match &frame.samples {
            Samples::PCM { samples } => {
                if !samples.is_empty() {
                    self.asr_client.send_audio(&samples)?;
                } else {
                    tracing::debug!(track_id = %frame.track_id, "AsrProcessor: empty PCM samples");
                }
            }
            _ => {
                tracing::debug!(track_id = %frame.track_id, "AsrProcessor: skipping non-PCM frame");
            }
        }
        Ok(())
    }
}
