//! Ringback / line-state detection as a media `Processor`.
//!
//! The audio path only does O(1) work per sample: samples are written into a
//! fixed ring (see [`super::ring`]) and, once per detection interval, a snapshot
//! of the 6 s window is handed to the process-wide inference pool (see
//! [`super::pool`]). No model evaluation, no dynamic buffer shifting and no
//! thread spawning happens on the media path.

use super::model_cache::{self, RingbackModel};
use super::pool::{self, RingbackPool};
use super::ring::AudioRing;
use super::state::{TrackHandle, TrackInferenceState};
use crate::RingbackDetectionOption;
use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::media::{AudioFrame, Samples};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub(crate) const SAMPLE_RATE: u32 = 16000;
/// Model input window: 6 s at 16 kHz.
pub(crate) const MODEL_NUM_SAMPLES: usize = 96_000;
pub(crate) const DEFAULT_WEIGHTS_PATH: &str = "./telcoclassifier_weights.bin";
/// Environment override for the process-wide inference thread count.
const LANES_ENV: &str = "RINGBACK_INFERENCE_THREADS";

pub(crate) const CLASS_NAMES: &[&str] = &[
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
// Audio processor
// ---------------------------------------------------------------------------

pub struct RingbackDetectionProcessor {
    cancel_token: CancellationToken,
    /// Sliding 6 s window, written in O(1) per sample.
    ring: AudioRing,
    num_samples_per_interval: usize,
    num_samples_min_buffer: usize,
    samples_since_last_inference: usize,
    total_accumulated: usize,
    early_media_received: Arc<AtomicBool>,
    first_detection_done: bool,
    model: Arc<RingbackModel>,
    pool: Arc<RingbackPool>,
    handle: Arc<TrackHandle>,
}

impl RingbackDetectionProcessor {
    pub fn new(
        track_id: String,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        option: RingbackDetectionOption,
        refer: Option<bool>,
    ) -> Result<Self> {
        let interval_secs = option.detection_interval_secs.unwrap_or(2.0).max(0.05);
        let min_buffer_secs = option.min_buffer_secs.unwrap_or(6.0).max(0.0);
        let num_samples_per_interval = (SAMPLE_RATE as f32 * interval_secs) as usize;
        let num_samples_min_buffer = (SAMPLE_RATE as f32 * min_buffer_secs) as usize;

        let weights_path = option
            .model_weights_path
            .clone()
            .unwrap_or_else(|| DEFAULT_WEIGHTS_PATH.to_string());
        // Shared across every call; loaded (and leaked) at most once per path.
        let model = model_cache::load(&weights_path)?;

        let pool = pool::global(resolve_max_lanes(option.inference_threads));

        let handle = TrackHandle::new(TrackInferenceState::new(
            track_id.clone(),
            event_sender.clone(),
            option.confidence_threshold.unwrap_or(0.5),
            option.on_state_change_only.unwrap_or(true),
            option.sliding_window_size.unwrap_or(6).max(1),
            option.final_confidence_threshold.unwrap_or(0.9),
            refer,
        ));

        let early_media_received = Arc::new(AtomicBool::new(false));
        spawn_cancel_watcher(
            track_id,
            cancel_token.clone(),
            // Subscribe eagerly: a `Ringing` event published right after the
            // processor is built must not be lost to task-start latency.
            event_sender.subscribe(),
            early_media_received.clone(),
            handle.clone(),
        );

        Ok(Self {
            cancel_token,
            ring: AudioRing::new(MODEL_NUM_SAMPLES),
            num_samples_per_interval,
            num_samples_min_buffer,
            samples_since_last_inference: 0,
            total_accumulated: 0,
            early_media_received,
            first_detection_done: false,
            model,
            pool,
            handle,
        })
    }
}

/// Upper bound for inference threads: call option > env > half the cores.
///
/// The pool only spawns threads on demand (and retires idle ones), so this is
/// a ceiling on inference CPU, not a pre-allocated thread count.
fn resolve_max_lanes(option: Option<usize>) -> usize {
    option
        .or_else(|| {
            std::env::var(LANES_ENV)
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or_else(|| num_cpus::get() / 2)
        .max(1)
}

/// Stops detection on answer/cancel and emits the final verdict.
fn spawn_cancel_watcher(
    track_id: String,
    cancel_token: CancellationToken,
    mut rx: tokio::sync::broadcast::Receiver<SessionEvent>,
    early_media_received: Arc<AtomicBool>,
    handle: Arc<TrackHandle>,
) {
    let watch_token = cancel_token.child_token();
    let stop_token = cancel_token;
    crate::spawn(async move {
        loop {
            tokio::select! {
                _ = watch_token.cancelled() => break,
                received = rx.recv() => match received {
                    Ok(SessionEvent::Ringing { early_media: true, .. }) => {
                        early_media_received.store(true, Ordering::Release);
                    }
                    Ok(SessionEvent::Answer { track_id: answered, .. }) if answered == track_id => {
                        debug!(track_id, "answer received, stopping ringback detection");
                        stop_token.cancel();
                        break;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        // Without this branch a lagging receiver would wedge the
                        // `select!` branch and never observe the answer.
                        debug!(track_id, skipped, "ringback watcher lagged behind events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        handle.request_finalize();
    });
}

impl Drop for RingbackDetectionProcessor {
    fn drop(&mut self) {
        // Same semantics as before: closing the processor finalizes whatever
        // was inferred. Idempotent, so a prior answer/cancel is a no-op.
        self.handle.request_finalize();
    }
}

impl Processor for RingbackDetectionProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // Nothing to do once a final verdict was emitted (very common: the
        // detection finishes long before the call does) or the track ended.
        if self.handle.is_finished() || self.cancel_token.is_cancelled() {
            return Ok(());
        }

        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };
        if samples.is_empty() {
            return Ok(());
        }

        self.ring.push_i16(samples);
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

        // The model expects exactly one 6 s window: shorter inputs mis-shape
        // the GRU front-end (it falls back to identity reshape and returns
        // garbage), so never run on a partial window.
        if !self.ring.is_full() {
            return Ok(());
        }

        // At most one inference in flight per call. If the pool is still
        // working on the previous window, this interval is stale: drop it
        // instead of queueing (this is the overload valve).
        if !self.handle.begin_job() {
            return Ok(());
        }

        let mut buffer = self.pool.acquire_buffer();
        self.ring.snapshot(&mut buffer);
        // Releases the gate itself if the job is rejected (queue full).
        self.pool
            .submit(self.model.clone(), self.handle.clone(), buffer);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ringback_detection::test_support;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    fn build(
        track: &str,
        option: RingbackDetectionOption,
    ) -> (RingbackDetectionProcessor, EventSender, CancellationToken) {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        let token = CancellationToken::new();
        let processor = RingbackDetectionProcessor::new(
            track.to_string(),
            token.clone(),
            tx.clone(),
            option,
            None,
        )
        .expect("processor");
        (processor, tx, token)
    }

    /// Detection only starts once the call reports early media (183 with SDP),
    /// mirroring the production flow where the watcher sets the flag from the
    /// `Ringing` event.
    async fn announce_early_media(tx: &EventSender, track: &str) {
        let _ = tx.send(SessionEvent::Ringing {
            track_id: track.to_string(),
            timestamp: 0,
            early_media: true,
            refer: None,
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    fn frames(pcm: &[i16], samples_per_frame: usize) -> Vec<AudioFrame> {
        pcm.chunks(samples_per_frame)
            .map(|chunk| AudioFrame {
                track_id: "caller".to_string(),
                samples: Samples::PCM {
                    samples: chunk.to_vec(),
                },
                sample_rate: SAMPLE_RATE,
                channels: 1,
                ..Default::default()
            })
            .collect()
    }

    fn require_weights() -> bool {
        if test_support::weights_path().is_some() {
            true
        } else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            false
        }
    }

    fn collect_ringback_events(
        rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
        sink: &Mutex<Vec<(String, String, f32, bool)>>,
    ) {
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::RingbackState {
                track_id,
                state,
                confidence,
                is_final,
                ..
            } = event
            {
                sink.lock()
                    .unwrap()
                    .push((track_id, state, confidence, is_final));
            }
        }
    }

    #[test]
    fn missing_weights_fail_fast_without_leaking() {
        let (tx, _rx) = tokio::sync::broadcast::channel(8);
        let err = RingbackDetectionProcessor::new(
            "t".into(),
            CancellationToken::new(),
            tx,
            RingbackDetectionOption {
                model_weights_path: Some("./no-such-weights.bin".into()),
                ..Default::default()
            },
            None,
        )
        .err()
        .expect("missing weights must be an error");
        assert!(format!("{err:#}").contains("no-such-weights.bin"));
    }

    #[test]
    fn lanes_resolution_prefers_option_then_env_and_defaults_to_half_the_cores() {
        assert_eq!(resolve_max_lanes(Some(1)), 1);
        assert_eq!(resolve_max_lanes(Some(0)), 1);
        let default = resolve_max_lanes(None);
        assert_eq!(default, (num_cpus::get() / 2).max(1));
        assert!(default >= 1 && default <= num_cpus::get());
    }

    #[tokio::test]
    async fn audio_path_is_o1_per_sample() {
        if !require_weights() {
            return;
        }
        // 60 s of audio at 20 ms/frame. The old rotation-based window needed
        // ~8 s of CPU for this (~13 % of a core per ringing call); the ring
        // needs a fraction of a millisecond.
        let (mut p, tx, _token) = build(
            "perf",
            RingbackDetectionOption {
                detection_interval_secs: Some(2.0),
                min_buffer_secs: Some(0.5),
                ..Default::default()
            },
        );
        announce_early_media(&tx, "perf").await;

        let mut audio = frames(&test_support::pcm_cn_ringback(60.0), 320);
        let start = Instant::now();
        for frame in audio.iter_mut() {
            p.process_frame(frame).unwrap();
        }
        let elapsed = start.elapsed();
        eprintln!(
            "processed 60 s of audio in {:.2} ms",
            elapsed.as_secs_f64() * 1000.0
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "audio path is doing O(window) work per sample: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn frame_processing_stops_once_finalized() {
        if !require_weights() {
            return;
        }
        let (mut p, tx, token) = build(
            "t",
            RingbackDetectionOption {
                detection_interval_secs: Some(0.5),
                min_buffer_secs: Some(1.0),
                ..Default::default()
            },
        );
        announce_early_media(&tx, "t").await;

        let mut warmup = frames(&test_support::pcm_cn_ringback(8.0), 320);
        for frame in warmup.iter_mut() {
            p.process_frame(frame).unwrap();
        }
        token.cancel();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(p.handle.is_finished(), "cancel must finalize the handle");

        let mut after = frames(&test_support::pcm_cn_ringback(60.0), 320);
        let start = Instant::now();
        for frame in after.iter_mut() {
            p.process_frame(frame).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "cancelled processor still doing work: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn detects_cn_ringback_end_to_end() {
        if !require_weights() {
            return;
        }
        let (mut p, tx, token) = build(
            "caller",
            RingbackDetectionOption {
                detection_interval_secs: Some(1.0),
                min_buffer_secs: Some(6.0),
                confidence_threshold: Some(0.5),
                on_state_change_only: Some(true),
                ..Default::default()
            },
        );
        let mut rx = tx.subscribe();
        announce_early_media(&tx, "caller").await;

        let mut audio = frames(&test_support::pcm_cn_ringback(20.0), 320);
        for frame in audio.iter_mut() {
            p.process_frame(frame).unwrap();
        }

        let events = Mutex::new(Vec::new());
        let deadline = Instant::now() + Duration::from_secs(30);
        while events.lock().unwrap().is_empty() && Instant::now() < deadline {
            collect_ringback_events(&mut rx, &events);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        token.cancel();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            collect_ringback_events(&mut rx, &events);
            if events.lock().unwrap().iter().any(|e| e.3) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let events = events.into_inner().unwrap();
        eprintln!("ringback events: {events:?}");
        let ringing = events
            .iter()
            .find(|(_, state, _, _)| state == "ringing")
            .expect("should detect the Chinese 450 Hz ringback as ringing");
        assert!(ringing.2 > 0.5, "confidence {}", ringing.2);

        let final_event = events
            .iter()
            .find(|(_, _, _, is_final)| *is_final)
            .expect("cancelling must emit a final verdict");
        assert_eq!(final_event.0, "caller");
        assert_eq!(final_event.1, "ringing");
    }

    #[tokio::test]
    async fn detects_real_ringback_fixture() {
        if !require_weights() {
            return;
        }
        let Some(pcm) = test_support::load_wav("fixtures/sample.wav") else {
            eprintln!("skipping: fixtures/sample.wav missing");
            return;
        };

        let (mut p, tx, token) = build(
            "caller",
            RingbackDetectionOption {
                detection_interval_secs: Some(1.0),
                min_buffer_secs: Some(6.0),
                confidence_threshold: Some(0.5),
                on_state_change_only: Some(false),
                ..Default::default()
            },
        );
        let mut rx = tx.subscribe();
        announce_early_media(&tx, "caller").await;

        let mut audio = frames(&pcm, 320);
        for frame in audio.iter_mut() {
            p.process_frame(frame).unwrap();
        }

        let events = Mutex::new(Vec::new());
        let deadline = Instant::now() + Duration::from_secs(30);
        while events.lock().unwrap().is_empty() && Instant::now() < deadline {
            collect_ringback_events(&mut rx, &events);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        token.cancel();
        tokio::time::sleep(Duration::from_millis(200)).await;
        collect_ringback_events(&mut rx, &events);

        let events = events.into_inner().unwrap();
        eprintln!("fixture events: {events:?}");
        assert!(
            events.iter().any(|(_, state, _, _)| state == "ringing"),
            "the colour-ringback fixture must be detected as ringing: {events:?}"
        );
    }

    #[tokio::test]
    async fn dropping_the_processor_emits_the_final_verdict() {
        if !require_weights() {
            return;
        }
        // The old implementation finalized when the inference channel closed;
        // dropping the processor must keep doing that.
        let (mut p, tx, _token) = build(
            "caller",
            RingbackDetectionOption {
                detection_interval_secs: Some(1.0),
                min_buffer_secs: Some(6.0),
                ..Default::default()
            },
        );
        let mut rx = tx.subscribe();
        announce_early_media(&tx, "caller").await;

        let mut audio = frames(&test_support::pcm_cn_ringback(14.0), 320);
        for frame in audio.iter_mut() {
            p.process_frame(frame).unwrap();
        }

        let events = Mutex::new(Vec::new());
        let deadline = Instant::now() + Duration::from_secs(30);
        while events.lock().unwrap().is_empty() && Instant::now() < deadline {
            collect_ringback_events(&mut rx, &events);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !events.lock().unwrap().is_empty(),
            "expected a detection before dropping the processor"
        );

        drop(p);
        tokio::time::sleep(Duration::from_millis(200)).await;
        collect_ringback_events(&mut rx, &events);

        let events = events.into_inner().unwrap();
        let finals: Vec<_> = events.iter().filter(|event| event.3).collect();
        eprintln!("drop events: {events:?}");
        assert_eq!(
            finals.len(),
            1,
            "dropping must emit exactly one final verdict"
        );
        assert_eq!(finals[0].1, "ringing");
    }

    #[tokio::test]
    async fn detects_busy_tone_not_ringing() {
        if !require_weights() {
            return;
        }
        let (mut p, tx, token) = build(
            "caller",
            RingbackDetectionOption {
                detection_interval_secs: Some(1.0),
                min_buffer_secs: Some(6.0),
                confidence_threshold: Some(0.5),
                on_state_change_only: Some(true),
                ..Default::default()
            },
        );
        let mut rx = tx.subscribe();
        announce_early_media(&tx, "caller").await;

        let mut audio = frames(&test_support::pcm_cn_busy(16.0), 320);
        for frame in audio.iter_mut() {
            p.process_frame(frame).unwrap();
        }
        let events = Mutex::new(Vec::new());
        let deadline = Instant::now() + Duration::from_secs(30);
        while events.lock().unwrap().is_empty() && Instant::now() < deadline {
            collect_ringback_events(&mut rx, &events);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        token.cancel();
        tokio::time::sleep(Duration::from_millis(200)).await;
        collect_ringback_events(&mut rx, &events);

        let events = events.into_inner().unwrap();
        eprintln!("busy events: {events:?}");
        assert!(
            events
                .iter()
                .any(|(_, state, conf, _)| state == "busy_tone" && *conf > 0.5),
            "450 Hz 350/350 must classify as busy_tone: {events:?}"
        );
    }

    #[tokio::test]
    async fn overload_skips_intervals_without_unbounded_growth() {
        if !require_weights() {
            return;
        }
        // 12 calls ringing at once with a 0.2 s interval: jobs must be skipped
        // or dropped instead of queueing without bound.
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        let mut procs = Vec::new();
        for i in 0..12 {
            let track = format!("t{i}");
            let (processor, _, _) = build(
                &track,
                RingbackDetectionOption {
                    detection_interval_secs: Some(0.2),
                    min_buffer_secs: Some(0.5),
                    ..Default::default()
                },
            );
            procs.push(processor);
        }
        announce_early_media(&tx, "t0").await;
        // `build` creates one sender per call; announce on each of them.
        for p in procs.iter_mut() {
            p.early_media_received.store(true, Ordering::Release);
        }

        let mut audio = frames(&test_support::pcm_cn_ringback(10.0), 320);
        for frame in audio.iter_mut() {
            for p in procs.iter_mut() {
                p.process_frame(frame).unwrap();
            }
        }
        let skipped: u64 = procs.iter().map(|p| p.handle.skipped()).sum();
        let stats = pool::global(1).stats();
        eprintln!("overload: skipped={skipped} pool={stats:?}");
        assert!(
            stats.queued <= stats.max_lanes * 8,
            "queue must stay bounded: {stats:?}"
        );
        assert!(
            skipped > 0 || stats.dropped_full > 0,
            "overload must be visible in the counters: {stats:?}"
        );
    }
}

#[cfg(test)]
mod file_probe {
    //! Ad-hoc recognizer for a real call recording (WAV, PCM16 or A-law).
    //!
    //! ```text
    //! RINGBACK_TEST_WAV=/path/to/call.wav cargo test --release \
    //!   --features ringback-detection --lib recognize_wav_file -- --ignored --nocapture
    //! ```

    use super::pool::LaneRuntime;
    use super::*;
    use crate::media::ringback_detection::test_support;
    use audio_codec::pcma::PcmaDecoder;
    use audio_codec::{Decoder, resampler::resample};
    use std::time::{Duration, Instant};

    struct Wav {
        format: u16,
        sample_rate: u32,
        channels: u16,
        data: Vec<u8>,
    }

    fn read_wav(path: &str) -> Wav {
        let bytes = std::fs::read(path).expect("read wav");
        assert_eq!(&bytes[0..4], b"RIFF", "not a RIFF file");
        assert_eq!(&bytes[8..12], b"WAVE", "not a WAVE file");
        let mut format = 0u16;
        let mut sample_rate = 0u32;
        let mut channels = 0u16;
        let mut bits = 0u16;
        let mut data = Vec::new();
        let mut pos = 12;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let body = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
            match id {
                b"fmt " => {
                    format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                    channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                    sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
                }
                b"data" => data = body.to_vec(),
                _ => {}
            }
            pos += 8 + size + (size & 1);
        }
        println!(
            "{}: format={format} {channels}ch {sample_rate}Hz {bits}bit, {:.1}s",
            path,
            data.len() as f64
                / channels.max(1) as f64
                / (if bits == 8 { 1.0 } else { 2.0 })
                / sample_rate as f64
        );
        Wav {
            format,
            sample_rate,
            channels,
            data,
        }
    }

    /// Decodes to mono 16 kHz PCM16, i.e. what the media pipeline feeds the
    /// processor (INTERNAL_SAMPLERATE).
    fn pcm16_mono_16k(wav: &Wav) -> Vec<i16> {
        let interleaved: Vec<i16> = match wav.format {
            1 => wav
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect(),
            6 => {
                let mut decoder = PcmaDecoder::new();
                let mut out = vec![0i16; wav.data.len()];
                let written = decoder.decode_into(&wav.data, &mut out).expect("a-law");
                out.truncate(written);
                out
            }
            other => panic!("unsupported wav format tag {other}"),
        };
        let channels = wav.channels.max(1) as usize;
        let mono: Vec<i16> = if channels == 1 {
            interleaved
        } else {
            interleaved
                .chunks_exact(channels)
                .map(|frame| {
                    (frame.iter().map(|&s| s as i32).sum::<i32>() / channels as i32) as i16
                })
                .collect()
        };
        if wav.sample_rate == SAMPLE_RATE {
            mono
        } else {
            resample(&mono, wav.sample_rate, SAMPLE_RATE)
        }
    }

    #[test]
    #[ignore = "manual probe: run with --ignored --nocapture"]
    fn recognize_wav_file() {
        let Some(model) = test_support::model() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        let path = std::env::var("RINGBACK_TEST_WAV")
            .unwrap_or_else(|_| "202611161102037988410016215.wav".to_string());
        if !std::path::Path::new(&path).is_file() {
            eprintln!("skipping: {path} not found (set RINGBACK_TEST_WAV)");
            return;
        }

        let decode_started = Instant::now();
        let wav = read_wav(&path);
        let pcm = pcm16_mono_16k(&wav);
        let audio_secs = pcm.len() as f64 / SAMPLE_RATE as f64;
        println!(
            "decoded to {audio_secs:.2}s mono 16kHz in {:.1} ms",
            decode_started.elapsed().as_secs_f64() * 1000.0
        );

        // Same cadence as the processor defaults: 6 s window, 2 s interval,
        // and the same state machine (`TrackInferenceState`), driven one
        // window at a time so nothing is skipped.
        let (tx, mut rx) = tokio::sync::broadcast::channel(1024);
        let mut state = TrackInferenceState::new("probe".to_string(), tx, 0.5, true, 6, 0.9, None);
        let mut lane = LaneRuntime::new(&model);
        let mut ring = AudioRing::new(MODEL_NUM_SAMPLES);
        let mut snapshot = Vec::with_capacity(MODEL_NUM_SAMPLES);
        let hop = SAMPLE_RATE as usize * 2;

        // Warm up: the first inference of a lane prepares the weights.
        let _ = lane.infer(&vec![0.0f32; MODEL_NUM_SAMPLES]);

        let mut infer_total = Duration::ZERO;
        let mut infer_max = Duration::ZERO;
        let mut audio_path = Duration::ZERO;
        let mut frames = 0usize;
        let mut rows: Vec<(f64, String, f32, f64)> = Vec::new();
        let mut events: Vec<(f64, String, f32, bool)> = Vec::new();

        let mut offset = 0usize;
        while offset + MODEL_NUM_SAMPLES <= pcm.len() {
            let t_audio = Instant::now();
            ring.push_i16(&pcm[offset..offset + hop]);
            if ring.is_full() {
                ring.snapshot(&mut snapshot);
            }
            audio_path += t_audio.elapsed();
            if !ring.is_full() {
                offset += hop;
                continue;
            }

            let t = Instant::now();
            let done = state.run(&mut lane, &snapshot);
            let elapsed = t.elapsed();
            infer_total += elapsed;
            infer_max = infer_max.max(elapsed);
            frames += 1;

            while let Ok(event) = rx.try_recv() {
                if let SessionEvent::RingbackState {
                    state,
                    confidence,
                    is_final,
                    ..
                } = event
                {
                    events.push((
                        offset as f64 / SAMPLE_RATE as f64,
                        state,
                        confidence,
                        is_final,
                    ));
                }
            }

            // Report the raw per-window result (same argmax the state uses).
            let probs = lane.infer(&snapshot);
            let (idx, &confidence) = probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap();
            rows.push((
                offset as f64 / SAMPLE_RATE as f64,
                CLASS_NAMES[idx].to_string(),
                confidence,
                elapsed.as_secs_f64() * 1000.0,
            ));
            offset += hop;
            if done {
                break;
            }
        }
        // The processor keeps detecting after a non-final state, so only stop
        // early on the high-confidence finalization above.
        let _ = &mut offset;

        println!(
            "\n{:<9} {:<16} {:>7} {:>7}",
            "end@(s)", "state", "conf", "ms"
        );
        for (end, state, conf, ms) in &rows {
            println!("{:<9.1} {:<16} {:>7.4} {:>7.1}", end, state, conf, ms);
        }

        let infer_secs = infer_total.as_secs_f64();
        let n = frames.max(1) as f64;
        println!("\nwindows            : {frames}  (6 s window, 2 s hop)");
        println!("audio duration     : {audio_secs:.2} s");
        println!(
            "inference total    : {:.3} s (avg {:.2} ms, max {:.2} ms)",
            infer_secs,
            infer_secs * 1000.0 / n,
            infer_max.as_secs_f64() * 1000.0
        );
        println!(
            "audio path total   : {:.2} ms (RTF {:.6})",
            audio_path.as_secs_f64() * 1000.0,
            audio_path.as_secs_f64() / audio_secs
        );
        println!(
            "RTF per window     : {:.5}  (inference / 6 s of audio)",
            infer_secs / (n * 6.0)
        );
        println!(
            "RTF streaming      : {:.5}  (inference / 2 s interval = core share per ringing call)",
            infer_secs / (n * 2.0)
        );
        println!(
            "RTF whole file     : {:.5}  (all inference / {audio_secs:.1} s)",
            infer_secs / audio_secs
        );
        println!(
            "throughput         : {:.1}x real time per lane (single thread)",
            audio_secs / infer_secs
        );

        println!("\nemitted events (threshold 0.5, window 6, onStateChangeOnly, final >= 0.9):");
        for (at, state, conf, is_final) in &events {
            println!("  @{at:>6.1}s  {state:<16} conf={conf:.4} final={is_final}");
        }

        assert!(frames > 0);
    }
}
