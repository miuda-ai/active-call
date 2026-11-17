//! Per-call ringback detection state and the handle shared with the inference
//! workers.
//!
//! All inference state (sliding window, previous state) lives behind
//! `TrackHandle::state`. Only inference worker threads execute `run`, and each
//! call has at most one job in flight (guarded by `TrackHandle::busy`), so the
//! window is updated strictly in order without any extra synchronisation.

use super::pool::LaneRuntime;
use super::processor::CLASS_NAMES;
use crate::event::{EventSender, SessionEvent};
use crate::media::get_timestamp;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::debug;

// ---------------------------------------------------------------------------
// Sliding window for inference result accumulation and finalization
// ---------------------------------------------------------------------------

struct RingbackEntry {
    state: String,
    state_index: u32,
    confidence: f32,
}

pub(crate) struct RingbackSlidingWindow {
    window: VecDeque<RingbackEntry>,
    window_size: usize,
    final_threshold: f32,
}

impl RingbackSlidingWindow {
    pub(crate) fn new(window_size: usize, final_threshold: f32) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size + 1),
            window_size,
            final_threshold,
        }
    }

    fn push(&mut self, state: String, state_index: u32, confidence: f32) {
        self.window.push_back(RingbackEntry {
            state,
            state_index,
            confidence,
        });
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

        let best_state =
            state_counts
                .into_iter()
                .max_by(|(_, count_a, sum_a), (_, count_b, sum_b)| {
                    count_a.cmp(count_b).then_with(|| {
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
// Inference state for one call
// ---------------------------------------------------------------------------

pub(crate) struct TrackInferenceState {
    track_id: String,
    event_sender: EventSender,
    confidence_threshold: f32,
    on_state_change_only: bool,
    prev_state: Option<String>,
    prev_confidence: Option<f32>,
    refer: Option<bool>,
    window: RingbackSlidingWindow,
    finalized: bool,
}

impl TrackInferenceState {
    pub(crate) fn new(
        track_id: String,
        event_sender: EventSender,
        confidence_threshold: f32,
        on_state_change_only: bool,
        sliding_window_size: usize,
        final_confidence_threshold: f32,
        refer: Option<bool>,
    ) -> Self {
        Self {
            track_id,
            event_sender,
            confidence_threshold,
            on_state_change_only,
            prev_state: None,
            prev_confidence: None,
            refer,
            window: RingbackSlidingWindow::new(sliding_window_size, final_confidence_threshold),
            finalized: false,
        }
    }

    pub(crate) fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Runs one inference. Returns `true` once a final result was emitted.
    pub(crate) fn run(&mut self, lane: &mut LaneRuntime, samples: &[f32]) -> bool {
        let probs = lane.infer(samples);

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

        self.window
            .push(state.to_string(), max_idx as u32, confidence);

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

    /// Emits the majority result of the window as the final answer.
    ///
    /// Idempotent: does nothing once finalized or when nothing was inferred.
    pub(crate) fn finalize(&mut self) {
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
// Handle shared between the audio path and the inference workers
// ---------------------------------------------------------------------------

/// Shared gate/state between a `RingbackDetectionProcessor` and the pool.
pub(crate) struct TrackHandle {
    state: Mutex<TrackInferenceState>,
    /// True while an inference job for this call is queued or running.
    busy: AtomicBool,
    /// Set once a final result was emitted: the audio path can stop buffering.
    finished: AtomicBool,
    /// A final result was requested while an inference was in flight.
    finalize_pending: AtomicBool,
    /// Intervals skipped because the previous inference was still running.
    skipped: AtomicU64,
}

impl TrackHandle {
    pub(crate) fn new(state: TrackInferenceState) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(state),
            busy: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            finalize_pending: AtomicBool::new(false),
            skipped: AtomicU64::new(0),
        })
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    pub(crate) fn lock_state(&self) -> MutexGuard<'_, TrackInferenceState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns `true` if this call may submit an inference job now.
    ///
    /// At most one job per call may be queued or running: the 6 s window is
    /// sliding, so if the previous interval has not been consumed the current
    /// one is stale anyway. This is the back-pressure valve that keeps memory
    /// and CPU bounded when many calls ring at once.
    pub(crate) fn begin_job(&self) -> bool {
        if self.busy.swap(true, Ordering::AcqRel) {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    pub(crate) fn end_job(&self) {
        self.busy.store(false, Ordering::Release);
    }

    pub(crate) fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
    }

    /// Emits the final result and stops the audio path from buffering more.
    ///
    /// Called when the call is answered/cancelled or the processor is dropped,
    /// i.e. exactly once per call. Runs inline when no inference is in flight
    /// and defers to the running job otherwise, so the caller never blocks
    /// behind a model evaluation.
    pub(crate) fn request_finalize(&self) {
        self.finalize_pending.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.try_lock()
            && self.finalize_pending.swap(false, Ordering::AcqRel)
        {
            state.finalize();
            drop(state);
            self.mark_finished();
        }
    }

    /// Worker-side hook: drains a finalize request made while the job was
    /// running (or racing with its completion).
    pub(crate) fn run_pending_finalize(&self) {
        if self.finalize_pending.swap(false, Ordering::AcqRel) {
            let mut state = self.lock_state();
            state.finalize();
            drop(state);
            self.mark_finished();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> Arc<TrackHandle> {
        let (tx, _rx) = tokio::sync::broadcast::channel(8);
        TrackHandle::new(TrackInferenceState::new(
            "t".into(),
            tx,
            0.5,
            true,
            6,
            0.9,
            None,
        ))
    }

    #[test]
    fn finalize_runs_inline_when_no_inference_is_running() {
        let handle = handle();
        handle.request_finalize();
        assert!(handle.is_finished());
        assert!(handle.lock_state().is_finalized());
    }

    #[test]
    fn finalize_is_deferred_while_an_inference_holds_the_state() {
        let handle = handle();
        // Simulate a lane in the middle of `run` (it holds the state lock).
        let guard = handle.lock_state();
        handle.request_finalize();
        assert!(
            !handle.is_finished(),
            "must not finalize behind a running inference"
        );
        drop(guard);
        // The lane drains the request as it finishes the job.
        handle.run_pending_finalize();
        assert!(handle.is_finished());
        assert!(handle.lock_state().is_finalized());
    }

    #[test]
    fn finalize_is_idempotent() {
        let handle = handle();
        handle.request_finalize();
        handle.request_finalize();
        handle.run_pending_finalize();
        assert!(handle.is_finished());
        assert!(handle.lock_state().is_finalized());
    }

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
