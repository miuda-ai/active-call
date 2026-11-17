//! Elastic pool of ringback inference worker threads.
//!
//! Before this pool, every call ran its model evaluation via
//! `tokio::task::block_in_place` inside a per-track tokio task: it hijacked a
//! runtime worker thread for the whole evaluation and let unbounded 384 KB
//! audio buffers pile up in a channel. Under load that turns into runtime
//! stalls plus unbounded memory.
//!
//! The pool gives a fixed **CPU ceiling** while still adapting to bursts:
//!
//! * lanes are plain OS threads (never tokio's blocking pool, never one thread
//!   per call), shared by all calls;
//! * a lane is only spawned when work is waiting and no lane is free, and it
//!   retires after [`LANE_IDLE_TIMEOUT`] without work, so an idle process keeps
//!   no model scratch memory;
//! * the number of lanes never exceeds `max_lanes` (default: half the cores),
//!   so inference can use at most that many cores no matter how many calls
//!   ring at once;
//! * each lane owns its own classifier instance (the generated model keeps a
//!   non-`Sync` prepared-weight cache on aarch64) and reuses a
//!   `TelcoClassifierWorkspace` across inferences instead of reallocating it.
//!
//! Back-pressure is per call: a call may have at most one job queued or
//! running, and the global queue is bounded, so a slow pool only causes stale
//! windows to be skipped, never a memory blow-up.

use super::model::{TelcoClassifier, TelcoClassifierWorkspace};
use super::model_cache::RingbackModel;
use super::state::{TrackHandle, TrackInferenceState};
use lele::tensor::TensorView;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tracing::{trace, warn};

/// How long an idle lane waits before it exits and releases its scratch memory.
pub(crate) const LANE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-lane stack: the model builds fairly deep conv/im2col temporaries.
const LANE_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Classifier plus reusable scratch space, owned by exactly one worker thread.
pub(crate) struct LaneRuntime {
    classifier: TelcoClassifier<'static>,
    workspace: TelcoClassifierWorkspace,
    /// Number of inferences run on this lane (diagnostics/tests).
    inferences: u64,
}

impl LaneRuntime {
    pub(crate) fn new(model: &RingbackModel) -> Self {
        Self {
            classifier: model.classifier(),
            workspace: TelcoClassifierWorkspace::new(),
            inferences: 0,
        }
    }

    /// Runs one inference and returns the class probabilities.
    pub(crate) fn infer(&mut self, samples: &[f32]) -> Vec<f32> {
        let shape = [1, samples.len()];
        let input = TensorView::new(samples, &shape);
        let out = self
            .classifier
            .forward_with_workspace(&mut self.workspace, input);
        self.inferences += 1;
        out.data.to_vec()
    }

    #[cfg(test)]
    pub(crate) fn inferences(&self) -> u64 {
        self.inferences
    }
}

struct Job {
    model: Arc<RingbackModel>,
    handle: Arc<TrackHandle>,
    samples: Vec<f32>,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    in_flight: usize,
    max_in_flight: usize,
}

#[derive(Default)]
struct Stats {
    submitted: AtomicU64,
    completed: AtomicU64,
    dropped_full: AtomicU64,
    infer_nanos_total: AtomicU64,
    infer_nanos_max: AtomicU64,
}

/// Snapshot of pool counters, for diagnostics and tests.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Configured upper bound for worker threads.
    pub max_lanes: usize,
    /// Worker threads alive right now.
    pub live_lanes: usize,
    /// Highest number of live worker threads seen.
    pub peak_lanes: usize,
    pub submitted: u64,
    pub completed: u64,
    pub dropped_full: u64,
    pub queued: usize,
    pub in_flight: usize,
    pub max_in_flight: usize,
    pub avg_infer_us: u64,
    pub max_infer_us: u64,
}

/// Elastic inference pool. Create one per process via [`global`].
pub(crate) struct RingbackPool {
    self_weak: Weak<RingbackPool>,
    /// Upper bound for worker threads; also the CPU ceiling (in cores).
    max_lanes: usize,
    idle_timeout: Duration,
    max_queue: usize,
    queue: Mutex<Queue>,
    cv: Condvar,
    stopping: AtomicBool,
    /// Threads currently alive (reserved + running).
    live: AtomicUsize,
    peak: AtomicUsize,
    spawn_failed: AtomicBool,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    stats: Stats,
    /// Recycled 384 KB audio buffers.
    buffers: Mutex<Vec<Vec<f32>>>,
}

static GLOBAL: OnceLock<Arc<RingbackPool>> = OnceLock::new();

/// Returns the process-wide pool, creating it on first use.
///
/// `max_lanes` only takes effect on the first call; later calls with a
/// different value keep the original ceiling and log a warning once.
pub(crate) fn global(max_lanes: usize) -> Arc<RingbackPool> {
    let requested = max_lanes.max(1);
    let pool = GLOBAL.get_or_init(|| RingbackPool::new(requested));
    if pool.max_lanes() != requested {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!(
                max_lanes = pool.max_lanes(),
                requested,
                "ringback inference pool is process-wide; keeping the first configured ceiling"
            );
        }
    }
    pool.clone()
}

impl RingbackPool {
    pub(crate) fn new(max_lanes: usize) -> Arc<Self> {
        Self::new_with(max_lanes, LANE_IDLE_TIMEOUT)
    }

    pub(crate) fn new_with(max_lanes: usize, idle_timeout: Duration) -> Arc<Self> {
        let max_lanes = max_lanes.max(1);
        Arc::new_cyclic(|weak| Self {
            self_weak: weak.clone(),
            max_lanes,
            idle_timeout,
            max_queue: max_lanes * 8,
            queue: Mutex::new(Queue::default()),
            cv: Condvar::new(),
            stopping: AtomicBool::new(false),
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            spawn_failed: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
            stats: Stats::default(),
            buffers: Mutex::new(Vec::new()),
        })
    }

    /// Configured upper bound for worker threads.
    pub(crate) fn max_lanes(&self) -> usize {
        self.max_lanes
    }

    /// Worker threads alive right now.
    pub(crate) fn live_lanes(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// Pool counters for diagnostics/metrics.
    #[allow(dead_code)]
    pub(crate) fn stats(&self) -> PoolStats {
        let queue = self.lock_queue();
        let completed = self.stats.completed.load(Ordering::Relaxed);
        let total = self.stats.infer_nanos_total.load(Ordering::Relaxed);
        PoolStats {
            max_lanes: self.max_lanes,
            live_lanes: self.live_lanes(),
            peak_lanes: self.peak.load(Ordering::Relaxed),
            submitted: self.stats.submitted.load(Ordering::Relaxed),
            completed,
            dropped_full: self.stats.dropped_full.load(Ordering::Relaxed),
            queued: queue.jobs.len(),
            in_flight: queue.in_flight,
            max_in_flight: queue.max_in_flight,
            avg_infer_us: match completed {
                0 => 0,
                n => total / n / 1_000,
            },
            max_infer_us: self.stats.infer_nanos_max.load(Ordering::Relaxed) / 1_000,
        }
    }

    fn lock_queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes a recycled audio buffer, or a fresh one on a cold pool.
    pub(crate) fn acquire_buffer(&self) -> Vec<f32> {
        self.buffers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .unwrap_or_default()
    }

    fn recycle_buffer(&self, mut buffer: Vec<f32>) {
        let mut buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if buffers.len() < self.max_queue {
            buffer.clear();
            buffers.push(buffer);
        }
    }

    /// Spawns one lane if the ceiling allows it.
    ///
    /// Returns `false` only when the OS refused to create the thread.
    fn spawn_lane(&self) -> bool {
        match self.reserve_lane_slot() {
            Some(slot) => slot,
            None => return !self.spawn_failed.load(Ordering::Acquire),
        };
        let weak = self.self_weak.clone();
        let spawned = std::thread::Builder::new()
            .name(format!(
                "ringback-infer-{}",
                self.live.load(Ordering::Acquire)
            ))
            .stack_size(LANE_STACK_BYTES)
            .spawn(move || worker_loop(weak));
        match spawned {
            Ok(handle) => {
                let mut threads = self
                    .threads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Reap lanes that already retired so this vector stays small.
                let mut index = 0;
                while index < threads.len() {
                    if threads[index].is_finished() {
                        let finished = threads.swap_remove(index);
                        let _ = finished.join();
                    } else {
                        index += 1;
                    }
                }
                threads.push(handle);
                self.peak
                    .fetch_max(self.live.load(Ordering::Acquire), Ordering::Relaxed);
                true
            }
            Err(err) => {
                self.live.fetch_sub(1, Ordering::AcqRel);
                self.spawn_failed.store(true, Ordering::Release);
                warn!(error = %err, "failed to spawn a ringback inference lane");
                false
            }
        }
    }

    /// Books a lane slot, or `None` when the ceiling is already reached.
    fn reserve_lane_slot(&self) -> Option<()> {
        loop {
            let live = self.live.load(Ordering::Acquire);
            if live >= self.max_lanes {
                return None;
            }
            if self
                .live
                .compare_exchange_weak(live, live + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(());
            }
        }
    }

    /// Enqueues one inference. Never blocks and never grows without bound.
    ///
    /// Takes ownership of `samples` and always releases the call's in-flight
    /// gate, so a dropped job cannot wedge a call's detection.
    pub(crate) fn submit(
        &self,
        model: Arc<RingbackModel>,
        handle: Arc<TrackHandle>,
        samples: Vec<f32>,
    ) -> bool {
        let job = Job {
            model,
            handle,
            samples,
        };

        // Degenerate case: the process cannot create threads any more. Run
        // inline so detection still works, just without a CPU ceiling.
        if self.spawn_failed.load(Ordering::Acquire) {
            self.run_inline(job);
            return true;
        }

        // Cold pool (idle process, or every lane just retired): start one lane
        // before queueing so the job cannot be stranded.
        if self.live_lanes() == 0 && !self.spawn_lane() {
            self.run_inline(job);
            return true;
        }

        let mut queue = self.lock_queue();
        if queue.jobs.len() >= self.max_queue {
            drop(queue);
            let dropped = self.stats.dropped_full.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                warn!(
                    dropped,
                    max_lanes = self.max_lanes,
                    max_queue = self.max_queue,
                    "ringback inference queue full, skipping detection interval"
                );
            }
            self.recycle_buffer(job.samples);
            job.handle.end_job();
            return false;
        }
        queue.jobs.push_back(job);
        // Close the race with a lane retiring between the check above and the
        // push: a lane only retires while the queue is empty at that moment.
        let orphaned = self.live_lanes() == 0;
        drop(queue);
        if orphaned && !self.spawn_lane() {
            if let Some(job) = self.lock_queue().jobs.pop_back() {
                self.recycle_buffer(job.samples);
                job.handle.end_job();
            }
            return false;
        }
        self.stats.submitted.fetch_add(1, Ordering::Relaxed);
        self.cv.notify_one();
        true
    }

    fn run_inline(&self, job: Job) {
        let started = Instant::now();
        let mut lanes = HashMap::new();
        run_job(&mut lanes, &job.model, &job.handle, &job.samples);
        self.record(started.elapsed());
        self.recycle_buffer(job.samples);
    }

    fn record(&self, elapsed: Duration) {
        let nanos = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.stats.completed.fetch_add(1, Ordering::Relaxed);
        self.stats
            .infer_nanos_total
            .fetch_add(nanos, Ordering::Relaxed);
        self.stats
            .infer_nanos_max
            .fetch_max(nanos, Ordering::Relaxed);
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
}

impl Drop for RingbackPool {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.cv.notify_all();
        let handles = match self.threads.get_mut() {
            Ok(handles) => std::mem::take(handles),
            Err(poisoned) => std::mem::take(poisoned.into_inner()),
        };
        for handle in handles {
            let _ = handle.join();
        }
    }
}

fn worker_loop(pool: Weak<RingbackPool>) {
    let mut lanes: HashMap<usize, LaneRuntime> = HashMap::new();
    loop {
        let Some(pool) = pool.upgrade() else {
            return;
        };

        let (job, backlog) = {
            let mut queue = pool.lock_queue();
            loop {
                if let Some(job) = queue.jobs.pop_front() {
                    queue.in_flight += 1;
                    queue.max_in_flight = queue.max_in_flight.max(queue.in_flight);
                    break (job, queue.jobs.len() >= pool.live_lanes());
                }
                if pool.is_stopping() {
                    pool.live.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
                let (guard, waited) = pool
                    .cv
                    .wait_timeout(queue, pool.idle_timeout)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                queue = guard;
                if waited.timed_out() && queue.jobs.is_empty() {
                    // Nothing to do for a while: release this lane (and its
                    // ~16 MB of model scratch) until the next burst.
                    pool.live.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
            }
        };

        if backlog {
            // More queued work than live lanes: scale out (bounded by the
            // configured ceiling).
            pool.spawn_lane();
        }

        let Job {
            model,
            handle,
            samples,
        } = job;
        let started = Instant::now();
        run_job(&mut lanes, &model, &handle, &samples);
        let elapsed = started.elapsed();
        {
            let mut queue = pool.lock_queue();
            queue.in_flight = queue.in_flight.saturating_sub(1);
        }
        pool.record(elapsed);
        pool.recycle_buffer(samples);
        trace!(
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            "ringback inference done"
        );
    }
}

fn run_job(
    lanes: &mut HashMap<usize, LaneRuntime>,
    model: &RingbackModel,
    handle: &TrackHandle,
    samples: &[f32],
) {
    {
        let mut state: std::sync::MutexGuard<'_, TrackInferenceState> = handle.lock_state();
        if !state.is_finalized() {
            let lane = lanes
                .entry(model.id())
                .or_insert_with(|| LaneRuntime::new(model));
            if state.run(lane, samples) {
                handle.mark_finished();
            }
        }
    }
    // Drain a finalize that arrived while this job was running. Still holding
    // the job gate, so no other job for this call can interleave.
    handle.run_pending_finalize();
    handle.end_job();
    // Close the race with a finalize requested between the check above and
    // `end_job`: `request_finalize` only runs inline when it can take the state
    // lock, which is impossible while a job holds it, so re-check.
    handle.run_pending_finalize();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SessionEvent;
    use crate::media::ringback_detection::test_support;

    fn test_state(track: &str, tx: &crate::event::EventSender) -> TrackInferenceState {
        TrackInferenceState::new(track.to_string(), tx.clone(), 0.5, true, 6, 0.9, None)
    }

    fn wait_completed(pool: &RingbackPool, completed: u64) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while pool.stats().completed < completed && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            pool.stats().completed >= completed,
            "expected {completed} jobs, got {:?}",
            pool.stats()
        );
    }

    fn wait_for(pool: &RingbackPool, what: &str, check: impl Fn(&RingbackPool) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !check(pool) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            check(pool),
            "timed out waiting for {what}: {:?}",
            pool.stats()
        );
    }

    fn submit_job(
        pool: &RingbackPool,
        model: &Arc<RingbackModel>,
        tx: &crate::event::EventSender,
        samples: &[f32],
    ) -> Arc<TrackHandle> {
        let handle = TrackHandle::new(test_state("t", tx));
        assert!(handle.begin_job());
        pool.submit(model.clone(), handle.clone(), samples.to_vec());
        handle
    }

    #[test]
    fn pool_starts_empty_and_spawns_lazily() {
        let pool = RingbackPool::new(3);
        assert_eq!(pool.max_lanes(), 3);
        assert_eq!(pool.live_lanes(), 0, "no thread before the first job");
    }

    #[test]
    fn global_pool_is_shared() {
        let a = global(2);
        let b = global(2);
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn empty_pool_drops_and_joins_cleanly() {
        let pool = RingbackPool::new(2);
        assert_eq!(pool.stats().completed, 0);
        drop(pool); // must not deadlock even with no lanes
    }

    #[test]
    fn busy_call_is_skipped_instead_of_queued() {
        let (tx, _rx) = tokio::sync::broadcast::channel::<SessionEvent>(8);
        let handle = TrackHandle::new(test_state("t", &tx));

        assert!(handle.begin_job(), "first interval may submit");
        assert!(!handle.begin_job(), "second interval is skipped while busy");
        assert_eq!(handle.skipped(), 1);

        handle.end_job();
        assert!(handle.begin_job(), "gate reopens after the job ends");
        handle.end_job();
    }

    #[test]
    fn finalize_after_inference_marks_handle_finished() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let (tx, _rx) = tokio::sync::broadcast::channel::<SessionEvent>(8);
        let pool = RingbackPool::new(1);
        let mut samples = Vec::new();
        test_support::window(&test_support::pcm_cn_ringback(8.0), &mut samples);

        let handle = submit_job(&pool, &model, &tx, &samples);
        wait_completed(&pool, 1);

        handle.request_finalize();
        assert!(handle.is_finished(), "finalize must mark the call finished");
        assert_eq!(pool.stats().max_in_flight, 1);
    }

    #[test]
    fn grows_with_backlog_up_to_the_ceiling() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let (tx, _rx) = tokio::sync::broadcast::channel::<SessionEvent>(64);
        let pool = RingbackPool::new(3);
        let mut samples = Vec::new();
        test_support::window(&test_support::pcm_cn_ringback(6.5), &mut samples);

        for _ in 0..12 {
            submit_job(&pool, &model, &tx, &samples);
        }
        wait_completed(&pool, 12);

        let stats = pool.stats();
        eprintln!("growth stats: {stats:?}");
        assert!(
            stats.peak_lanes >= 2,
            "a backlog should scale the pool out: {stats:?}"
        );
        assert!(stats.peak_lanes <= 3, "the ceiling must hold: {stats:?}");
        assert!(stats.live_lanes <= 3);
    }

    #[test]
    fn idle_lanes_retire_and_release_resources() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let (tx, _rx) = tokio::sync::broadcast::channel::<SessionEvent>(8);
        let pool = RingbackPool::new_with(2, Duration::from_millis(100));
        let mut samples = Vec::new();
        test_support::window(&test_support::pcm_cn_ringback(6.5), &mut samples);

        submit_job(&pool, &model, &tx, &samples);
        wait_completed(&pool, 1);
        assert!(pool.live_lanes() >= 1);

        wait_for(&pool, "all lanes to retire", |pool| pool.live_lanes() == 0);
        // The pool must still work after retiring everything.
        submit_job(&pool, &model, &tx, &samples);
        wait_completed(&pool, 2);
    }

    #[test]
    fn at_most_one_job_in_flight_per_lane() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let (tx, _rx) = tokio::sync::broadcast::channel::<SessionEvent>(8);
        let pool = RingbackPool::new(2);
        let mut samples = Vec::new();
        test_support::window(&test_support::pcm_cn_ringback(6.5), &mut samples);

        for _ in 0..8 {
            submit_job(&pool, &model, &tx, &samples);
        }
        wait_completed(&pool, 8);

        let stats = pool.stats();
        assert_eq!(stats.completed, 8);
        assert!(
            stats.max_in_flight <= 2,
            "a pool capped at 2 lanes ran {} jobs concurrently",
            stats.max_in_flight
        );
        assert_eq!(stats.queued, 0);
    }

    #[test]
    fn bounded_queue_drops_instead_of_growing() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let (tx, _rx) = tokio::sync::broadcast::channel::<SessionEvent>(1024);
        let pool = RingbackPool::new(1);
        let mut samples = Vec::new();
        test_support::window(&test_support::pcm_cn_ringback(6.2), &mut samples);

        let mut accepted = 0u64;
        let mut dropped = 0u64;
        for i in 0..64 {
            let handle = TrackHandle::new(test_state(&format!("t{i}"), &tx));
            assert!(handle.begin_job());
            if pool.submit(model.clone(), handle.clone(), samples.clone()) {
                accepted += 1;
            } else {
                dropped += 1;
                // A dropped job must release the in-flight gate, otherwise the
                // call would stop detecting forever.
                assert!(handle.begin_job(), "gate must be released on drop");
                handle.end_job();
            }
        }
        assert!(
            accepted <= 1 + 8 + 1,
            "queue must stay bounded, accepted {accepted}"
        );
        assert!(dropped > 0, "overload must be dropped, not queued");

        wait_completed(&pool, accepted);
        assert_eq!(pool.stats().dropped_full, dropped);
    }

    #[test]
    fn lane_matches_one_shot_reference_and_is_repeatable() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let mut window = Vec::new();
        test_support::window(&test_support::pcm_cn_ringback(7.0), &mut window);
        assert_eq!(window.len(), 96_000);

        let mut lane = LaneRuntime::new(&model);
        let first = lane.infer(&window);
        // Second call reuses the workspace: results must be bit-identical.
        let second = lane.infer(&window);
        assert_eq!(first, second, "workspace reuse changed the result");
        assert_eq!(lane.inferences(), 2);

        // Reference is the previous one-shot entry point (`forward`, which
        // allocates a fresh workspace every call).
        let shape = [1, window.len()];
        let reference: Vec<f32> = model
            .classifier()
            .forward(TensorView::new(&window, &shape))
            .data
            .to_vec();
        assert_eq!(
            first, reference,
            "pool lane must produce the same probabilities as the old path"
        );

        // Known-good classification of a Chinese ringback tone.
        let (idx, &confidence) = first
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(
            crate::media::ringback_detection::processor::CLASS_NAMES[idx],
            "ringing"
        );
        assert!(confidence > 0.6, "confidence {confidence}");
    }

    #[test]
    fn single_lane_never_runs_two_jobs_at_once() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let (tx, _rx) = tokio::sync::broadcast::channel::<SessionEvent>(8);
        let pool = RingbackPool::new(1);
        let mut samples = Vec::new();
        test_support::window(&test_support::pcm_cn_ringback(6.2), &mut samples);

        for i in 0..4 {
            submit_job(&pool, &model, &tx, &samples);
            wait_completed(&pool, i + 1);
        }
        let stats = pool.stats();
        assert_eq!(stats.max_lanes, 1);
        assert_eq!(stats.max_in_flight, 1);
        assert_eq!(stats.peak_lanes, 1);
    }
}
