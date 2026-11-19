use crate::event::{EventReceiver, SessionEvent};
use crate::media::{AudioFrame, PcmBuf, Sample, Samples, processor::Processor};
use anyhow::Result;
use audio_codec::BoxedResampler;
use nnnoiseless::DenoiseState;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::broadcast::error::RecvError;

pub struct NoiseReducer {
    resampler_target: BoxedResampler,
    resampler_source: BoxedResampler,
    denoiser: Box<DenoiseState<'static>>,
    /// Flipped once the call is answered; denoising only runs after that.
    /// One relaxed atomic load per frame — no per-frame channel access.
    answered: Arc<AtomicBool>,
}

impl NoiseReducer {
    pub fn new(input_sample_rate: usize, answered: Arc<AtomicBool>) -> Self {
        let resampler48k =
            BoxedResampler::new(48000, input_sample_rate).expect("invalid sample rate");
        let resampler16k =
            BoxedResampler::new(input_sample_rate, 48000).expect("invalid sample rate");
        let denoiser = DenoiseState::new();
        Self {
            resampler_target: resampler48k,
            resampler_source: resampler16k,
            denoiser,
            answered,
        }
    }
}

/// Watches the session event channel and flips `answered` on the
/// `SessionEvent::Answer` broadcast — the same signal the ASR client's
/// `start_when_answer` gating uses. Must be called from a tokio context.
/// Lagged receivers enable denoise too (the Answer event may have been
/// among the skipped ones), matching the ASR wait helper's semantics.
pub fn spawn_answer_watcher(mut event_rx: EventReceiver, answered: Arc<AtomicBool>) {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(SessionEvent::Answer { .. }) => {
                    answered.store(true, Ordering::Relaxed);
                    break;
                }
                Err(RecvError::Lagged(_)) => {
                    answered.store(true, Ordering::Relaxed);
                    break;
                }
                Err(RecvError::Closed) => break,
                Ok(_) => {}
            }
        }
    });
}

impl Processor for NoiseReducer {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // Only denoise after the call is answered; before that (ringing,
        // early media) pass frames through untouched. One relaxed load per
        // frame — the answer watcher owns all event-channel access.
        if !self.answered.load(Ordering::Relaxed) {
            return Ok(());
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn noise_reducer(answered: bool) -> NoiseReducer {
        NoiseReducer::new(16000, Arc::new(AtomicBool::new(answered)))
    }

    fn make_frame() -> AudioFrame {
        AudioFrame {
            track_id: "test-track".to_string(),
            samples: Samples::PCM {
                samples: vec![100i16; 320],
            },
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        }
    }

    #[test]
    fn skips_frames_until_answered() {
        let mut reducer = noise_reducer(false);
        let mut frame = make_frame();
        let before = match &frame.samples {
            Samples::PCM { samples } => samples.clone(),
            _ => panic!("expected pcm"),
        };

        // Not answered yet: frame passes through byte-identical — no
        // resampling, no allocation, no denoising.
        reducer.process_frame(&mut frame).unwrap();
        match &frame.samples {
            Samples::PCM { samples } => assert_eq!(*samples, before),
            _ => panic!("expected pcm"),
        }
    }

    #[test]
    fn denoises_after_answered() {
        let mut reducer = noise_reducer(true);
        let mut frame = make_frame();
        reducer.process_frame(&mut frame).unwrap();
        // After the answer the frame is denoised: it goes through the
        // 16k->48k->16k resampling round trip and comes back at the same
        // length, PCM format preserved.
        match &frame.samples {
            Samples::PCM { samples } => assert_eq!(samples.len(), 320),
            _ => panic!("expected pcm"),
        }
    }

    #[test]
    fn answer_watcher_flips_flag() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, rx) = tokio::sync::broadcast::channel(64);
            let flag = Arc::new(AtomicBool::new(false));
            spawn_answer_watcher(rx, flag.clone());
            assert!(!flag.load(Ordering::Relaxed));

            tx.send(SessionEvent::Answer {
                track_id: "test-track".to_string(),
                timestamp: 0,
                sdp: "v=0".to_string(),
                refer: None,
            })
            .unwrap();
            // Wait until the watcher processes the event.
            for _ in 0..100 {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(flag.load(Ordering::Relaxed));
        });
    }
}
