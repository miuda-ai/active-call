use super::model::TelcoClassifier;
use crate::RingbackDetectionOption;
use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::media::{AudioFrame, Samples, get_timestamp};
use anyhow::{Context, Result};
use lele::tensor::TensorView;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

const SAMPLE_RATE: u32 = 16000;
const MODEL_NUM_SAMPLES: usize = 96000;
const CLASS_NAMES: &[&str] = &[
    "silence",
    "busy_tone",
    "fast_busy",
    "ringing",
    "empty_number",
    "out_of_service",
    "user_unavailable",
    "user_busy",
    "tts_voice",
    "answer_machine",
    "human_voice",
    "other",
];

// ---------------------------------------------------------------------------
// Sliding window for inference result accumulation and finalization
// ---------------------------------------------------------------------------

struct RingbackEntry {
    state: String,
    state_index: u32,
    confidence: f32,
}

struct RingbackSlidingWindow {
    window: VecDeque<RingbackEntry>,
    window_size: usize,
    final_threshold: f32,
}

impl RingbackSlidingWindow {
    fn new(window_size: usize, final_threshold: f32) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size + 1),
            window_size,
            final_threshold,
        }
    }

    fn push(&mut self, state: String, state_index: u32, confidence: f32) {
        self.window.push_back(RingbackEntry { state, state_index, confidence });
        while self.window.len() > self.window_size {
            self.window.pop_front();
        }
    }

    /// Returns the entry with the highest confidence >= final_threshold, if any
    fn high_confidence_entry(&self) -> Option<&RingbackEntry> {
        let mut best: Option<&RingbackEntry> = None;
        for entry in &self.window {
            if entry.confidence >= self.final_threshold {
                match best {
                    None => best = Some(entry),
                    Some(b) if entry.confidence > b.confidence => best = Some(entry),
                    _ => {}
                }
            }
        }
        best
    }

    /// Determines the best result via majority vote across the window.
    /// Tiebreak: higher average confidence wins.
    fn best_result(&self) -> Option<(String, u32, f32)> {
        if self.window.is_empty() {
            return None;
        }

        let mut state_counts: Vec<(String, usize, f32)> = Vec::new();
        for entry in &self.window {
            if let Some(existing) = state_counts.iter_mut().find(|(s, _, _)| s == &entry.state) {
                existing.1 += 1;
                existing.2 += entry.confidence;
            } else {
                state_counts.push((entry.state.clone(), 1, entry.confidence));
            }
        }

        let best_state = state_counts
            .into_iter()
            .max_by(|(_, count_a, sum_a), (_, count_b, sum_b)| {
                count_a
                    .cmp(count_b)
                    .then_with(|| {
                        let avg_a = sum_a / *count_a as f32;
                        let avg_b = sum_b / *count_b as f32;
                        avg_a
                            .partial_cmp(&avg_b)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })?;

        self.window
            .iter()
            .find(|e| e.state == best_state.0)
            .map(|e| (e.state.clone(), e.state_index, e.confidence))
    }
}

// ---------------------------------------------------------------------------
// Inference task
// ---------------------------------------------------------------------------

struct InferenceTask {
    event_sender: EventSender,
    track_id: String,
    classifier: TelcoClassifier<'static>,
    confidence_threshold: f32,
    on_state_change_only: bool,
    prev_state: Option<String>,
    prev_confidence: Option<f32>,
    refer: Option<bool>,
    window: RingbackSlidingWindow,
    finalized: bool,
}

impl InferenceTask {
    fn run(&mut self, buf: &[f32]) -> bool {
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

        self.window.push(state.to_string(), max_idx as u32, confidence);

        if let Some(entry) = self.window.high_confidence_entry() {
            let prev = self.prev_state.take();
            let prev_conf = self.prev_confidence.take();
            self.emit(
                &entry.state,
                entry.state_index,
                entry.confidence,
                prev,
                prev_conf,
                true,
            );
            self.finalized = true;
            return true;
        }

        if confidence < self.confidence_threshold {
            return false;
        }

        let changed = self.prev_state.as_deref() != Some(state);
        if self.on_state_change_only && !changed {
            self.prev_state = Some(state.to_string());
            self.prev_confidence = Some(confidence);
            return false;
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

        self.emit(state, max_idx as u32, confidence, prev, prev_conf, false);

        self.prev_state = Some(state.to_string());
        self.prev_confidence = Some(confidence);
        false
    }

    fn finalize_and_stop(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;

        if let Some((state, state_index, confidence)) = self.window.best_result() {
            let prev = self.prev_state.take();
            let prev_conf = self.prev_confidence.take();

            debug!(
                track_id = self.track_id,
                state,
                confidence,
                prev_state = prev.as_deref(),
                "ringback state (final)"
            );

            self.emit(&state, state_index, confidence, prev, prev_conf, true);
        }
    }

    fn emit(
        &self,
        state: &str,
        state_index: u32,
        confidence: f32,
        prev_state: Option<String>,
        prev_confidence: Option<f32>,
        is_final: bool,
    ) {
        self.event_sender
            .send(SessionEvent::RingbackState {
                track_id: self.track_id.clone(),
                timestamp: get_timestamp(),
                state: state.to_string(),
                state_index,
                confidence,
                prev_state,
                prev_confidence,
                refer: self.refer,
                is_final,
            })
            .ok();
    }
}

// ---------------------------------------------------------------------------
// Audio processor
// ---------------------------------------------------------------------------

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
        let weights_data = std::fs::read(&weights_path).with_context(|| {
            format!(
                "Failed to load ringback model weights from {}",
                weights_path
            )
        })?;
        let weights: &'static [u8] = Box::leak(weights_data.into_boxed_slice());
        let classifier = TelcoClassifier::new(weights);

        let sliding_window_size = option.sliding_window_size.unwrap_or(6);
        let final_confidence_threshold = option.final_confidence_threshold.unwrap_or(0.9);

        let (inference_tx, mut inference_rx): (
            mpsc::UnboundedSender<Vec<f32>>,
            mpsc::UnboundedReceiver<Vec<f32>>,
        ) = mpsc::unbounded_channel();
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
                window: RingbackSlidingWindow::new(sliding_window_size, final_confidence_threshold),
                finalized: false,
            };
            loop {
                tokio::select! {
                    _ = task_token.cancelled() => {
                        task.finalize_and_stop();
                        break;
                    }
                    buf = inference_rx.recv() => {
                        match buf {
                            Some(buf) => {
                                let done = tokio::task::block_in_place(|| {
                                    task.run(&buf)
                                });
                                if done {
                                    break;
                                }
                            }
                            None => {
                                task.finalize_and_stop();
                                break;
                            }
                        }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sliding_window_push_and_size() {
        let mut sw = RingbackSlidingWindow::new(4, 0.9);
        assert_eq!(sw.window.len(), 0);

        sw.push("ringing".into(), 3, 0.7);
        assert_eq!(sw.window.len(), 1);

        sw.push("ringing".into(), 3, 0.8);
        sw.push("ringing".into(), 3, 0.75);
        sw.push("ringing".into(), 3, 0.6);
        assert_eq!(sw.window.len(), 4);

        sw.push("human_voice".into(), 10, 0.85);
        // window size is 4, so oldest entry (ringing 0.7) should be evicted
        assert_eq!(sw.window.len(), 4);

        // verify oldest was evicted: window should be [ringing(0.8), ringing(0.75), ringing(0.6), human_voice(0.85)]
        let entry_0 = &sw.window[0];
        assert_eq!(entry_0.state, "ringing");
        assert!((entry_0.confidence - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_high_confidence_entry_none() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        sw.push("ringing".into(), 3, 0.7);
        sw.push("busy_tone".into(), 1, 0.85);
        assert!(sw.high_confidence_entry().is_none());
    }

    #[test]
    fn test_high_confidence_entry_found() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        sw.push("ringing".into(), 3, 0.7);
        sw.push("busy_tone".into(), 1, 0.92);
        sw.push("ringing".into(), 3, 0.88);

        let entry = sw.high_confidence_entry().unwrap();
        assert_eq!(entry.state, "busy_tone");
        assert!((entry.confidence - 0.92).abs() < 1e-6);
    }

    #[test]
    fn test_high_confidence_picks_highest() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        sw.push("ringing".into(), 3, 0.95);
        sw.push("busy_tone".into(), 1, 0.92);

        let entry = sw.high_confidence_entry().unwrap();
        assert_eq!(entry.state, "ringing");
        assert!((entry.confidence - 0.95).abs() < 1e-6);
    }

    #[test]
    fn test_best_result_empty() {
        let sw = RingbackSlidingWindow::new(6, 0.9);
        assert!(sw.best_result().is_none());
    }

    #[test]
    fn test_best_result_majority_single_state() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        sw.push("ringing".into(), 3, 0.7);
        sw.push("ringing".into(), 3, 0.8);
        sw.push("ringing".into(), 3, 0.75);

        let (state, idx, _) = sw.best_result().unwrap();
        assert_eq!(state, "ringing");
        assert_eq!(idx, 3);
    }

    #[test]
    fn test_best_result_majority_wins() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        sw.push("ringing".into(), 3, 0.7);
        sw.push("ringing".into(), 3, 0.8);
        sw.push("busy_tone".into(), 1, 0.85);
        sw.push("human_voice".into(), 10, 0.9);
        sw.push("ringing".into(), 3, 0.75);

        // ringing appears 3 times, busy_tone 1, human_voice 1
        let (state, idx, _) = sw.best_result().unwrap();
        assert_eq!(state, "ringing");
        assert_eq!(idx, 3);
    }

    #[test]
    fn test_best_result_tiebreak_by_avg_confidence() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        // ringing: 2 entries, avg = (0.8 + 0.7) / 2 = 0.75
        sw.push("ringing".into(), 3, 0.8);
        sw.push("ringing".into(), 3, 0.7);
        // busy_tone: 2 entries, avg = (0.9 + 0.85) / 2 = 0.875 → higher avg
        sw.push("busy_tone".into(), 1, 0.9);
        sw.push("busy_tone".into(), 1, 0.85);

        let (state, idx, _) = sw.best_result().unwrap();
        assert_eq!(state, "busy_tone");
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_best_result_tiebreak_same_avg_picks_first() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        // both have count=2 and same avg confidence
        sw.push("ringing".into(), 3, 0.8);
        sw.push("ringing".into(), 3, 0.8);
        sw.push("busy_tone".into(), 1, 0.8);
        sw.push("busy_tone".into(), 1, 0.8);

        let result = sw.best_result().unwrap();
        // max_by with tiebreaks: for equal counts and equal avg,
        // the order depends on the first-seen in iteration order
        // Either state is acceptable; we just verify it doesn't panic
        // and returns a valid entry
        assert!(result.0 == "ringing" || result.0 == "busy_tone");
    }

    #[test]
    fn test_window_eviction_affects_best_result() {
        let mut sw = RingbackSlidingWindow::new(3, 0.9);
        sw.push("ringing".into(), 3, 0.7);
        sw.push("ringing".into(), 3, 0.8);
        sw.push("ringing".into(), 3, 0.75);
        // window: [ringing(0.7), ringing(0.8), ringing(0.75)]
        let (state, _, _) = sw.best_result().unwrap();
        assert_eq!(state, "ringing");

        // push a 4th entry → evicts oldest ringing(0.7)
        sw.push("busy_tone".into(), 1, 0.95);
        // window: [ringing(0.8), ringing(0.75), busy_tone(0.95)]
        let (state, _, _) = sw.best_result().unwrap();
        // ringing: 2, busy_tone: 1
        assert_eq!(state, "ringing");
    }

    #[test]
    fn test_finalize_after_shutdown_with_mixed_results() {
        let mut sw = RingbackSlidingWindow::new(6, 0.9);
        sw.push("silence".into(), 0, 0.6);
        sw.push("ringing".into(), 3, 0.7);
        sw.push("ringing".into(), 3, 0.8);
        sw.push("human_voice".into(), 10, 0.85);
        sw.push("ringing".into(), 3, 0.75);
        // ringing: 3, silence: 1, human_voice: 1
        let (state, idx, _) = sw.best_result().unwrap();
        assert_eq!(state, "ringing");
        assert_eq!(idx, 3);
    }
}
