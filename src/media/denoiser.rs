use crate::media::{AudioFrame, PcmBuf, Sample, Samples, processor::Processor};
use anyhow::Result;
use audio_codec::Resampler;
use nnnoiseless::DenoiseState;

pub struct NoiseReducer {
    resampler_target: Resampler,
    resampler_source: Resampler,
    denoiser: Box<DenoiseState<'static>>,
}

impl NoiseReducer {
    pub fn new(input_sample_rate: usize) -> Self {
        let resampler48k = Resampler::new(48000, input_sample_rate);
        let resampler16k = Resampler::new(input_sample_rate, 48000 as usize);
        let denoiser = DenoiseState::new();
        Self {
            resampler_target: resampler48k,
            resampler_source: resampler16k,
            denoiser,
        }
    }
}
unsafe impl Send for NoiseReducer {}
unsafe impl Sync for NoiseReducer {}

impl Processor for NoiseReducer {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // If empty frame, nothing to do
        if frame.samples.is_empty() {
            return Ok(());
        }

        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };
        let samples = self.resampler_source.resample(samples);
        let input_size = samples.len();

        let output_padding_size = input_size + DenoiseState::FRAME_SIZE;
        let mut output_buf = vec![0.0; output_padding_size];
        let input_f32: Vec<f32> = samples.iter().map(|&s| s.into()).collect();

        let mut offset = 0;
        let mut buf;

        while offset < input_size {
            let remaining_size = input_size - offset;
            let chunk_len = remaining_size.min(DenoiseState::FRAME_SIZE);
            let end_offset = offset + chunk_len;

            let input_chunk = if chunk_len < DenoiseState::FRAME_SIZE {
                buf = vec![0.0; DenoiseState::FRAME_SIZE];
                buf[..chunk_len].copy_from_slice(&input_f32[offset..end_offset]);
                &buf
            } else {
                &input_f32[offset..end_offset]
            };

            // Process the current frame
            self.denoiser.process_frame(
                &mut output_buf[offset..offset + DenoiseState::FRAME_SIZE],
                &input_chunk,
            );

            offset += chunk_len;
        }

        let samples = output_buf[..input_size]
            .iter()
            .map(|&s| s as Sample)
            .collect::<PcmBuf>();

        frame.samples = Samples::PCM {
            samples: self.resampler_target.resample(&samples),
        };

        Ok(())
    }
}
