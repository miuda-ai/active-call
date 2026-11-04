//! Packet loss concealment for decoded PCM, after ITU-T G.711 Appendix I.
//!
//! A lost frame is filled by repeating the last pitch period of the audio
//! before it, continuing the waveform where it stopped. The period's last
//! quarter is overlap-added with the quarter before it, so each repetition
//! runs into the next without a step. (Appendix I also smooths the seam
//! with the last audio played, by holding output back 3.75 ms; this plays
//! without delay, so that seam is only as smooth as the pitch estimate.)
//! Long losses fade out: the first 10 ms play at full level, then the level
//! drops by 20% per 10 ms and reaches silence 60 ms in. When real frames
//! resume, the first one is crossfaded with the continued synthesis.
//!
//! Codec independent: it works on whatever PCM the decoder produced, so it
//! covers G.711 and G.722, which have no concealment of their own.

use crate::media::PcmBuf;

/// Pitch range searched, in Hz (G.711 Appendix I uses 66-200 Hz).
const MIN_PITCH_HZ: u32 = 66;
const MAX_PITCH_HZ: u32 = 200;
/// Length of the window correlated when estimating the pitch.
const CORR_MS: u32 = 20;
/// Concealment plays at full level for this long, then fades.
const FULL_LEVEL_MS: u32 = 10;
/// Fade per 10 ms after `FULL_LEVEL_MS`; reaches silence at 60 ms.
const FADE_PER_10MS: f32 = 0.2;
/// Crossfade into the first real frame after a loss.
const RECOVER_MS: u32 = 4;

pub struct Concealer {
    sample_rate: u32,
    frame_len: usize,
    /// The most recent audio played, real or concealed.
    history: Vec<i16>,
    /// The pitch period being repeated during a loss.
    period: Vec<i16>,
    /// Read position in `period`.
    pos: usize,
    /// Samples concealed in the current loss; 0 when not concealing.
    concealed: usize,
}

impl Concealer {
    pub fn new() -> Self {
        Self {
            sample_rate: 0,
            frame_len: 0,
            history: Vec::new(),
            period: Vec::new(),
            pos: 0,
            concealed: 0,
        }
    }

    fn ms(&self, ms: u32) -> usize {
        (self.sample_rate * ms / 1000) as usize
    }

    fn history_len(&self) -> usize {
        self.ms(CORR_MS) + self.ms(1000 / MIN_PITCH_HZ) + self.frame_len
    }

    fn remember(&mut self, samples: &[i16]) {
        self.history.extend_from_slice(samples);
        let keep = self.history_len();
        if self.history.len() > keep {
            self.history.drain(..self.history.len() - keep);
        }
    }

    /// Record a frame that arrived. A frame ending a loss is crossfaded
    /// in place with the continued synthesis.
    pub fn on_frame(&mut self, samples: &mut [i16], sample_rate: u32) {
        if sample_rate != self.sample_rate || samples.len() != self.frame_len {
            // a new format: nothing earlier is usable to conceal with
            self.sample_rate = sample_rate;
            self.frame_len = samples.len();
            self.history.clear();
            self.concealed = 0;
        }
        if self.concealed > 0 {
            let len = self.ms(RECOVER_MS).min(samples.len());
            let tail = self.synthesize(len);
            for (i, (s, t)) in samples.iter_mut().zip(tail).enumerate() {
                let w = (i + 1) as f32 / (len + 1) as f32;
                *s = (*s as f32 * w + t as f32 * (1.0 - w)) as i16;
            }
            self.concealed = 0;
        }
        self.remember(samples);
    }

    /// A frame to play in place of a lost one, with its sample rate, or
    /// `None` before any audio has been seen to conceal from.
    pub fn conceal(&mut self) -> Option<(PcmBuf, u32)> {
        if self.frame_len == 0 || self.history.len() < self.history_len() {
            return None;
        }
        if self.concealed == 0 {
            self.start_period();
        }
        let mut out = self.synthesize(self.frame_len);
        let full = self.ms(FULL_LEVEL_MS);
        let per_10ms = self.ms(10).max(1);
        for (i, s) in out.iter_mut().enumerate() {
            let t = self.concealed + i;
            let gain = if t < full {
                1.0
            } else {
                (1.0 - FADE_PER_10MS * (t - full) as f32 / per_10ms as f32).max(0.0)
            };
            *s = (*s as f32 * gain) as i16;
        }
        self.concealed += out.len();
        self.remember(&out);
        Some((out, self.sample_rate))
    }

    /// Pick the pitch period to repeat from the end of the history.
    fn start_period(&mut self) {
        let min_lag = self.ms(1000 / MAX_PITCH_HZ).max(1);
        let max_lag = self.ms(1000 / MIN_PITCH_HZ);
        let win = self.ms(CORR_MS);
        let h = &self.history;
        let end = h.len();
        let target = &h[end - win..];
        let mut best = (min_lag, f32::MIN);
        for lag in min_lag..=max_lag.min(end - win) {
            let cand = &h[end - win - lag..end - lag];
            let (mut xy, mut yy) = (0f32, 0f32);
            for (x, y) in target.iter().zip(cand) {
                xy += *x as f32 * *y as f32;
                yy += *y as f32 * *y as f32;
            }
            let score = if yy > 0.0 { xy / yy.sqrt() } else { 0.0 };
            if score > best.1 {
                best = (lag, score);
            }
        }
        let p = best.0;
        let mut period = h[end - p..].to_vec();
        // the period wraps from its end, the last sample played, back to
        // its start: fade its last quarter into the quarter before its
        // start, the audio that led into the start
        let ola = p / 4;
        let before = &h[end - p - ola..end - p];
        for (i, (t, b)) in period[p - ola..].iter_mut().zip(before).enumerate() {
            let w = (i + 1) as f32 / (ola + 1) as f32;
            *t = (*t as f32 * (1.0 - w) + *b as f32 * w) as i16;
        }
        self.period = period;
        self.pos = 0;
    }

    fn synthesize(&mut self, len: usize) -> PcmBuf {
        if self.period.is_empty() {
            return vec![0; len];
        }
        (0..len)
            .map(|_| {
                let s = self.period[self.pos];
                self.pos = (self.pos + 1) % self.period.len();
                s
            })
            .collect()
    }
}

impl Default for Concealer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 16000;
    const FRAME: usize = 320;

    /// A 150 Hz tone, continuous across frames.
    fn tone(frame: usize) -> Vec<i16> {
        (0..FRAME)
            .map(|i| {
                let t = (frame * FRAME + i) as f32 / SR as f32;
                (8000.0 * (2.0 * std::f32::consts::PI * 150.0 * t).sin()) as i16
            })
            .collect()
    }

    fn rms(s: &[i16]) -> f32 {
        (s.iter().map(|x| (*x as f32).powi(2)).sum::<f32>() / s.len() as f32).sqrt()
    }

    fn max_step(s: &[i16]) -> i32 {
        s.windows(2)
            .map(|w| (w[1] as i32 - w[0] as i32).abs())
            .max()
            .unwrap()
    }

    fn primed() -> Concealer {
        let mut c = Concealer::new();
        for f in 0..5 {
            c.on_frame(&mut tone(f), SR);
        }
        c
    }

    #[test]
    fn nothing_to_conceal_from_before_audio() {
        let mut c = Concealer::new();
        assert!(c.conceal().is_none());
    }

    #[test]
    fn concealed_frame_continues_the_waveform() {
        let mut c = primed();
        let last = *tone(4).last().unwrap();
        let out = c.conceal().unwrap().0;
        assert_eq!(out.len(), FRAME);
        // close to the tone's own level, not silence
        assert!(rms(&out) > 0.5 * rms(&tone(5)), "rms {}", rms(&out));
        // no step at the seam or within: a 150 Hz tone at 8000 moves at
        // most ~470 per sample
        let mut seam = vec![last];
        seam.extend_from_slice(&out);
        assert!(max_step(&seam) < 1000, "step {}", max_step(&seam));
    }

    #[test]
    fn concealment_continues_from_any_phase() {
        // the loss may cut the waveform anywhere in its cycle: the
        // concealment must carry on from that point, across its own
        // repetitions too
        for offset in (0..FRAME).step_by(13) {
            let shifted = |frame: usize| -> Vec<i16> {
                (0..FRAME)
                    .map(|i| {
                        let t = (frame * FRAME + i + offset) as f32 / SR as f32;
                        (8000.0 * (2.0 * std::f32::consts::PI * 150.0 * t).sin()) as i16
                    })
                    .collect()
            };
            let mut c = Concealer::new();
            for f in 0..5 {
                c.on_frame(&mut shifted(f), SR);
            }
            let mut seam = vec![*shifted(4).last().unwrap()];
            seam.extend_from_slice(&c.conceal().unwrap().0);
            assert!(
                max_step(&seam) < 1000,
                "offset {offset}: step {}",
                max_step(&seam)
            );
        }
    }

    #[test]
    fn long_loss_fades_to_silence() {
        let mut c = primed();
        let frames: Vec<_> = (0..4).map(|_| c.conceal().unwrap().0).collect();
        assert!(rms(&frames[1]) < rms(&frames[0]));
        // 60 ms in, silence
        assert!(frames[3].iter().all(|s| *s == 0));
    }

    #[test]
    fn recovery_is_crossfaded() {
        let mut c = primed();
        let concealed = c.conceal().unwrap().0;
        // the real audio resumes out of phase with the concealment
        let mut real: Vec<i16> = tone(6).iter().map(|s| -s).collect();
        c.on_frame(&mut real, SR);
        let mut seam = vec![*concealed.last().unwrap()];
        seam.extend_from_slice(&real[..80]);
        assert!(max_step(&seam) < 2000, "step {}", max_step(&seam));
    }

    #[test]
    fn format_change_restarts_history() {
        let mut c = primed();
        c.on_frame(&mut vec![0; 160], 8000);
        assert!(c.conceal().is_none());
    }
}
