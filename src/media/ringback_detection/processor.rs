use super::model::TelcoClassifier;
use crate::event::{EventSender, SessionEvent};
use crate::media::{AudioFrame, Samples, get_timestamp};
use crate::media::processor::Processor;
use crate::RingbackDetectionOption;
use anyhow::{Context, Result};
use lele::tensor::TensorView;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

const SAMPLE_RATE: u32 = 16000;
const MODEL_NUM_SAMPLES: usize = 96000;
const CLASS_NAMES: &[&str] = &[
    "silence", "busy_tone", "fast_busy", "ringing",
    "empty_number", "out_of_service", "user_unavailable",
    "tts_voice", "answer_machine", "human_voice", "other",
];

struct InferenceTask {
    event_sender: EventSender,
    track_id: String,
    classifier: TelcoClassifier<'static>,
    confidence_threshold: f32,
    on_state_change_only: bool,
    prev_state: Option<String>,
    prev_confidence: Option<f32>,
    refer: Option<bool>,
}

impl InferenceTask {
    fn run(&mut self, buf: &[f32]) {
        let shape = [1, buf.len()];
        let input = TensorView::new(buf, &shape);
        let output = self.classifier.forward(input);
        let probs: Vec<f32> = output.data.to_vec();

        let mut max_idx = 0;
        let mut max_val = probs[0];
        for (i, &p) in probs.iter().enumerate().skip(1) {
            if p > max_val {
                max_val = p;
                max_idx = i;
            }
        }

        let state = CLASS_NAMES.get(max_idx).unwrap_or(&"unknown");
        let confidence = max_val;

        if confidence < self.confidence_threshold {
            return;
        }

        let changed = self.prev_state.as_deref() != Some(state);
        if self.on_state_change_only && !changed {
            self.prev_state = Some(state.to_string());
            self.prev_confidence = Some(confidence);
            return;
        }

        let prev = self.prev_state.take();
        let prev_conf = self.prev_confidence.take();

        debug!(
            track_id = self.track_id,
            state,
            confidence,
            prev_state = prev.as_deref(),
            "ringback state"
        );

        self.event_sender
            .send(SessionEvent::RingbackState {
                track_id: self.track_id.clone(),
                timestamp: get_timestamp(),
                state: state.to_string(),
                state_index: max_idx as u32,
                confidence,
                prev_state: prev,
                prev_confidence: prev_conf,
                refer: self.refer,
            })
            .ok();

        self.prev_state = Some(state.to_string());
        self.prev_confidence = Some(confidence);
    }
}

pub struct RingbackDetectionProcessor {
    cancel_token: CancellationToken,
    buffer: Vec<f32>,
    num_samples_per_interval: usize,
    num_samples_min_buffer: usize,
    samples_since_last_inference: usize,
    total_accumulated: usize,
    early_media_received: Arc<AtomicBool>,
    first_detection_done: bool,
    inference_tx: mpsc::UnboundedSender<Vec<f32>>,
}

impl RingbackDetectionProcessor {
    pub fn new(
        track_id: String,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        option: RingbackDetectionOption,
        refer: Option<bool>,
    ) -> Result<Self> {
        let interval_secs = option.detection_interval_secs.unwrap_or(2.0);
        let min_buffer_secs = option.min_buffer_secs.unwrap_or(6.0);
        let num_samples_per_interval = (SAMPLE_RATE as f32 * interval_secs) as usize;
        let num_samples_min_buffer = (SAMPLE_RATE as f32 * min_buffer_secs) as usize;

        let weights_path = option
            .model_weights_path
            .clone()
            .unwrap_or_else(|| "./telcoclassifier_weights.bin".to_string());
        let weights_data = std::fs::read(&weights_path)
            .with_context(|| format!("Failed to load ringback model weights from {}", weights_path))?;
        let weights: &'static [u8] = Box::leak(weights_data.into_boxed_slice());
        let classifier = TelcoClassifier::new(weights);

        let (inference_tx, mut inference_rx): (mpsc::UnboundedSender<Vec<f32>>, mpsc::UnboundedReceiver<Vec<f32>>) = mpsc::unbounded_channel();
        let task_event_sender = event_sender.clone();
        let task_track_id = track_id.clone();
        let confidence_threshold = option.confidence_threshold.unwrap_or(0.5);
        let on_state_change_only = option.on_state_change_only.unwrap_or(true);
        let task_token = cancel_token.child_token();

        crate::spawn(async move {
            let mut task = InferenceTask {
                event_sender: task_event_sender,
                track_id: task_track_id,
                classifier,
                confidence_threshold,
                on_state_change_only,
                prev_state: None,
                prev_confidence: None,
                refer,
            };
            loop {
                tokio::select! {
                    _ = task_token.cancelled() => break,
                    Some(buf) = inference_rx.recv() => {
                        tokio::task::block_in_place(|| {
                            task.run(&buf);
                        });
                    }
                }
            }
        });

        let early_media_received = Arc::new(AtomicBool::new(false));
        let flag = early_media_received.clone();
        let mut rx = event_sender.subscribe();
        let watch_token = cancel_token.child_token();
        let stop_token = cancel_token.clone();
        let watch_track_id = track_id.clone();
        crate::spawn(async move {
            loop {
                tokio::select! {
                    _ = watch_token.cancelled() => break,
                    Ok(event) = rx.recv() => {
                        match &event {
                            SessionEvent::Ringing { early_media: true, .. } => {
                                flag.store(true, Ordering::Release);
                            }
                            SessionEvent::Answer { track_id, .. } if track_id == &watch_track_id => {
                                debug!(track_id = watch_track_id, "answer received, stopping ringback detection");
                                stop_token.cancel();
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Self {
            cancel_token,
            buffer: Vec::with_capacity(MODEL_NUM_SAMPLES),
            num_samples_per_interval,
            num_samples_min_buffer,
            samples_since_last_inference: 0,
            total_accumulated: 0,
            early_media_received,
            first_detection_done: false,
            inference_tx,
        })
    }
}

impl Processor for RingbackDetectionProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if self.cancel_token.is_cancelled() {
            return Ok(());
        }

        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };

        if samples.is_empty() {
            return Ok(());
        }

        for &s in samples {
            let v = s as f32 / 32768.0;
            if self.buffer.len() < MODEL_NUM_SAMPLES {
                self.buffer.push(v);
            } else {
                self.buffer.rotate_left(1);
                *self.buffer.last_mut().unwrap() = v;
            }
        }

        self.total_accumulated += samples.len();
        self.samples_since_last_inference += samples.len();

        let early = self.early_media_received.load(Ordering::Acquire);
        let enough_for_first = self.first_detection_done
            || (early && self.total_accumulated >= self.num_samples_min_buffer);

        if !enough_for_first {
            return Ok(());
        }

        if self.samples_since_last_inference < self.num_samples_per_interval {
            return Ok(());
        }

        self.samples_since_last_inference = 0;
        self.first_detection_done = true;

        if self.buffer.len() < 16000 {
            return Ok(());
        }

        let inference_buf = self.buffer.clone();
        self.inference_tx.send(inference_buf).ok();

        Ok(())
    }
}
