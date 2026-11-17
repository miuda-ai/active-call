//! Fixed-size audio ring used by the ringback detector.
//!
//! The detector needs a sliding 6 s window of 16 kHz audio (96 000 samples) for
//! every inference. The original implementation kept that window in a `Vec<f32>`
//! and called `rotate_left(1)` for **every incoming sample**, i.e. one 384 KB
//! memmove per sample at 16 kHz (~6 GB/s of memory traffic). Measured on an
//! Apple silicon core that is ~13 % of a CPU core for every call that is
//! ringing, burned on the media path.
//!
//! A ring buffer makes the per-sample cost O(1) and pays a single contiguous
//! memcpy per inference interval instead.

/// `i16 -> f32` conversion factor for the model input ([-1.0, 1.0)).
const SCALE: f32 = 1.0 / 32768.0;

pub(crate) struct AudioRing {
    buf: Box<[f32]>,
    /// Index of the next sample to write.
    pos: usize,
    /// Number of valid samples, saturating at `buf.len()`.
    len: usize,
}

impl AudioRing {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "audio ring capacity must be positive");
        Self {
            buf: vec![0.0; capacity].into_boxed_slice(),
            pos: 0,
            len: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == self.buf.len()
    }

    /// Appends PCM16 samples, overwriting the oldest ones once full.
    pub(crate) fn push_i16(&mut self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let cap = self.buf.len();
        let mut written = 0;
        while written < samples.len() {
            let space = cap - self.pos;
            let take = space.min(samples.len() - written);
            let src = &samples[written..written + take];
            let dst = &mut self.buf[self.pos..self.pos + take];
            for (d, &s) in dst.iter_mut().zip(src) {
                *d = s as f32 * SCALE;
            }
            self.pos += take;
            if self.pos == cap {
                self.pos = 0;
            }
            written += take;
        }
        self.len = (self.len + samples.len()).min(cap);
    }

    /// Copies the window into `out` in chronological order (oldest first).
    ///
    /// `out` is cleared and reused, so callers can keep one scratch buffer for
    /// the lifetime of the processor and avoid a fresh allocation per interval.
    pub(crate) fn snapshot(&self, out: &mut Vec<f32>) {
        out.clear();
        if self.len < self.buf.len() {
            // Not wrapped yet: [0..len] is already chronological.
            out.extend_from_slice(&self.buf[..self.len]);
        } else {
            out.extend_from_slice(&self.buf[self.pos..]);
            out.extend_from_slice(&self.buf[..self.pos]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The naive implementation the ring replaces: a growing `Vec` that
    /// shifts left by one sample once it reaches the model window size.
    struct NaiveWindow {
        buf: Vec<f32>,
        cap: usize,
    }

    impl NaiveWindow {
        fn new(cap: usize) -> Self {
            Self {
                buf: Vec::with_capacity(cap),
                cap,
            }
        }

        fn push_i16(&mut self, samples: &[i16]) {
            for &s in samples {
                let v = s as f32 * SCALE;
                if self.buf.len() < self.cap {
                    self.buf.push(v);
                } else {
                    self.buf.rotate_left(1);
                    *self.buf.last_mut().unwrap() = v;
                }
            }
        }
    }

    fn pseudo_random_pcm(len: usize, seed: u32) -> Vec<i16> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as i16
            })
            .collect()
    }

    #[test]
    fn empty_ring_snapshots_empty() {
        let ring = AudioRing::new(8);
        let mut out = vec![1.0, 2.0];
        ring.snapshot(&mut out);
        assert!(out.is_empty());
        assert_eq!(ring.len(), 0);
        assert!(!ring.is_full());
    }

    #[test]
    fn partial_window_is_chronological() {
        let mut ring = AudioRing::new(8);
        ring.push_i16(&[1, 2, 3]);
        let mut out = Vec::new();
        ring.snapshot(&mut out);
        assert_eq!(out, vec![1.0 / 32768.0, 2.0 / 32768.0, 3.0 / 32768.0]);
        assert_eq!(ring.len(), 3);
    }

    #[test]
    fn wraps_and_keeps_latest_window() {
        let mut ring = AudioRing::new(4);
        ring.push_i16(&[1, 2, 3, 4, 5, 6]);
        let mut out = Vec::new();
        ring.snapshot(&mut out);
        assert_eq!(
            out,
            [3i16, 4, 5, 6]
                .iter()
                .map(|&v| v as f32 / 32768.0)
                .collect::<Vec<_>>()
        );
        assert!(ring.is_full());
        assert_eq!(ring.len(), 4);
    }

    #[test]
    fn snapshot_matches_naive_shift_for_random_streams() {
        let cap = 64;
        let mut ring = AudioRing::new(cap);
        let mut naive = NaiveWindow::new(cap);
        let mut ring_out = Vec::new();
        let mut seed = 7u32;

        for chunk_len in [1usize, 3, 17, 64, 65, 128, 5, 320] {
            let pcm = pseudo_random_pcm(chunk_len, seed);
            seed = seed.wrapping_add(1);
            ring.push_i16(&pcm);
            naive.push_i16(&pcm);

            ring.snapshot(&mut ring_out);
            assert_eq!(
                ring_out, naive.buf,
                "snapshot diverged after pushing {chunk_len} samples"
            );
        }
    }

    #[test]
    fn large_stream_snapshot_is_bit_exact_vs_naive() {
        let cap = 9_600; // scaled-down model window
        let mut ring = AudioRing::new(cap);
        let mut naive = NaiveWindow::new(cap);
        let pcm = pseudo_random_pcm(cap * 3 + 137, 42);

        // Feed in 20 ms (320 sample) frames like the media path does.
        for frame in pcm.chunks(320) {
            ring.push_i16(frame);
            naive.push_i16(frame);
            if naive.buf.len() == cap {
                let mut out = Vec::new();
                ring.snapshot(&mut out);
                assert_eq!(out, naive.buf);
            }
        }
    }

    /// A/B of the two window implementations on the same machine/compiler.
    /// Ignored by default: it is a measurement, not a correctness gate.
    #[test]
    #[ignore = "perf comparison: run with --ignored --nocapture"]
    fn cpu_ab_old_rotate_vs_ring() {
        use crate::media::ringback_detection::test_support;
        use std::time::Instant;

        let pcm = test_support::pcm_cn_ringback(30.0);

        // Old implementation: 384 KB memmove per sample once the window fills.
        let mut legacy: Vec<f32> = Vec::with_capacity(96_000);
        let start = Instant::now();
        for chunk in pcm.chunks(320) {
            for &s in chunk {
                let value = s as f32 * SCALE;
                if legacy.len() < 96_000 {
                    legacy.push(value);
                } else {
                    legacy.rotate_left(1);
                    *legacy.last_mut().unwrap() = value;
                }
            }
        }
        let old = start.elapsed();

        // New implementation: O(1) per sample + one memcpy for the window.
        let mut ring = AudioRing::new(96_000);
        let mut out = Vec::new();
        let start = Instant::now();
        for chunk in pcm.chunks(320) {
            ring.push_i16(chunk);
        }
        ring.snapshot(&mut out);
        let new = start.elapsed();

        eprintln!(
            "30 s of audio: old rotate {old:?} ({:.1} % of a core) vs ring {new:?} \
             ({:.3} % of a core), {:.0}x faster",
            old.as_secs_f64() * 100.0 / 30.0,
            new.as_secs_f64() * 100.0 / 30.0,
            old.as_secs_f64() / new.as_secs_f64().max(f64::MIN_POSITIVE)
        );
        assert_eq!(out, legacy);
        assert!(
            new * 10 < old,
            "ring should be an order of magnitude faster"
        );
    }

    #[test]
    fn snapshot_reuses_output_allocation() {
        let mut ring = AudioRing::new(16);
        ring.push_i16(&pseudo_random_pcm(40, 3));
        let mut out = Vec::with_capacity(64);
        ring.snapshot(&mut out);
        let ptr = out.as_ptr();
        ring.push_i16(&[7; 4]);
        ring.snapshot(&mut out);
        assert_eq!(out.as_ptr(), ptr, "snapshot should reuse the buffer");
        assert_eq!(out.len(), 16);
    }
}
