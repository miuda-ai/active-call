//! Playout buffer for inbound RTP audio.
//!
//! Frames used to be decoded and forwarded the moment they arrived, so
//! network jitter went straight through to the bridged leg and every lost or
//! reordered packet became a hole. This buffer sits on a playout clock
//! instead: the owner calls [`PlayoutBuffer::tick`] once per packet time and
//! gets the next frame in sequence order, or [`Playout::Lost`] for a frame
//! that is missing, so the gap can be concealed (see [`super::plc`]).
//!
//! Playout starts once `target_frames` are buffered, which is the delay that
//! absorbs jitter. A frame missing while later ones are already buffered is
//! lost. A frame missing with nothing buffered may just be late: the buffer
//! conceals it without giving up on it, and rebuffers after
//! `max_conceal_frames` of that. Each such underrun leaves one more frame of
//! delay behind, which `max_frames` and the drift trim win back.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct PlayoutConfig {
    /// Frames buffered before playout starts.
    pub target_frames: usize,
    /// Frames buffered at most; beyond that the oldest are dropped.
    pub max_frames: usize,
    /// Consecutive frames concealed on an empty buffer before rebuffering.
    pub max_conceal_frames: usize,
    /// Ticks the buffer has to stay above `target_frames` before one frame
    /// is dropped to bring the delay back down.
    pub trim_after_ticks: usize,
}

impl Default for PlayoutConfig {
    fn default() -> Self {
        // at 20 ms frames: 40 ms of delay, up to 200 ms buffered, 100 ms of
        // concealment, trimmed after 2 s above target
        Self {
            target_frames: 2,
            max_frames: 10,
            max_conceal_frames: 5,
            trim_after_ticks: 100,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Playout<T> {
    Frame(T),
    /// A frame is due but missing: conceal it.
    Lost,
    /// Nothing to play (not started, or rebuffering after the stream paused).
    Idle,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PlayoutStats {
    pub received: u64,
    pub played: u64,
    /// Missing while later frames were buffered.
    pub lost: u64,
    /// Concealed on an empty buffer.
    pub underruns: u64,
    /// Arrived after their slot was played or concealed.
    pub late: u64,
    pub duplicates: u64,
    /// Dropped to keep the delay down.
    pub dropped: u64,
    pub resets: u64,
}

/// A sequence jump this large (about 2 s of 20 ms frames) is a new stream
/// (re-INVITE, sender restart), not loss or reordering.
const RESET_GAP: u64 = 100;

pub struct PlayoutBuffer<T> {
    config: PlayoutConfig,
    frames: BTreeMap<u64, T>,
    /// Extended sequence number of the next frame to play, while playing.
    next: Option<u64>,
    /// Extended form of the last sequence number seen, to unwrap the next.
    last_seq: Option<u64>,
    /// The sender's SSRC, when known.
    source: Option<u32>,
    concealed_run: usize,
    above_target_ticks: usize,
    stats: PlayoutStats,
}

impl<T> PlayoutBuffer<T> {
    pub fn new(config: PlayoutConfig) -> Self {
        Self {
            config,
            frames: BTreeMap::new(),
            next: None,
            last_seq: None,
            source: None,
            concealed_run: 0,
            above_target_ticks: 0,
            stats: PlayoutStats::default(),
        }
    }

    pub fn stats(&self) -> &PlayoutStats {
        &self.stats
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Extend a 16-bit sequence number to the value nearest the last one
    /// seen, so wraparound keeps ordering.
    fn extend(&self, seq: u16) -> u64 {
        let Some(last) = self.last_seq else {
            // start well clear of zero so a backward step can't underflow
            return (1 << 32) + seq as u64;
        };
        let base = last & !0xffff;
        [base.wrapping_sub(1 << 16), base, base + (1 << 16)]
            .into_iter()
            .map(|b| b + seq as u64)
            .min_by_key(|c| c.abs_diff(last))
            .unwrap()
    }

    fn reset(&mut self) {
        self.frames.clear();
        self.next = None;
        self.last_seq = None;
        self.concealed_run = 0;
        self.above_target_ticks = 0;
        self.stats.resets += 1;
    }

    /// Note the SSRC of the frames about to be pushed. A new SSRC is a new
    /// stream with its own random sequence numbers: one landing just behind
    /// the old stream's would otherwise be dropped as late until it caught
    /// up, which `RESET_GAP` doesn't cover.
    pub fn set_source(&mut self, ssrc: u32) {
        if self.source.is_some_and(|source| source != ssrc) {
            self.reset();
        }
        self.source = Some(ssrc);
    }

    pub fn push(&mut self, seq: u16, frame: T) {
        let mut ext = self.extend(seq);
        if let Some(last) = self.last_seq
            && ext.abs_diff(last) > RESET_GAP
        {
            self.reset();
            ext = self.extend(seq);
        }
        self.stats.received += 1;
        if self.last_seq.is_none_or(|last| ext > last) {
            self.last_seq = Some(ext);
        }
        if self.next.is_some_and(|next| ext < next) {
            self.stats.late += 1;
            return;
        }
        if self.frames.contains_key(&ext) {
            self.stats.duplicates += 1;
            return;
        }
        self.frames.insert(ext, frame);
        while self.frames.len() > self.config.max_frames {
            self.drop_oldest();
        }
    }

    fn drop_oldest(&mut self) {
        if let Some((seq, _)) = self.frames.pop_first() {
            self.stats.dropped += 1;
            if self.next.is_some() {
                self.next = Some(seq + 1);
            }
        }
    }

    pub fn tick(&mut self) -> Playout<T> {
        let next = match self.next {
            Some(next) => next,
            None => {
                if self.frames.len() < self.config.target_frames.max(1) {
                    return Playout::Idle;
                }
                *self.frames.keys().next().unwrap()
            }
        };

        // drift trim: buffered well past the target for long means the
        // delay grew (underruns, sender clock running fast); drop a frame
        if self.frames.len() > self.config.target_frames + 1 {
            self.above_target_ticks += 1;
        } else {
            self.above_target_ticks = 0;
        }
        let next = if self.above_target_ticks >= self.config.trim_after_ticks
            && self.frames.remove(&next).is_some()
        {
            self.stats.dropped += 1;
            self.above_target_ticks = 0;
            next + 1
        } else {
            next
        };

        if let Some(frame) = self.frames.remove(&next) {
            self.next = Some(next + 1);
            self.concealed_run = 0;
            self.stats.played += 1;
            return Playout::Frame(frame);
        }
        if !self.frames.is_empty() {
            // later frames are here, so this one is lost
            self.next = Some(next + 1);
            self.concealed_run += 1;
            self.stats.lost += 1;
            return Playout::Lost;
        }
        if self.concealed_run < self.config.max_conceal_frames {
            // nothing buffered: the frame may only be late, keep its slot
            self.next = Some(next);
            self.concealed_run += 1;
            self.stats.underruns += 1;
            return Playout::Lost;
        }
        // the stream paused (hold, DTX, sender gone): rebuffer
        self.next = None;
        self.concealed_run = 0;
        Playout::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer() -> PlayoutBuffer<u16> {
        PlayoutBuffer::new(PlayoutConfig::default())
    }

    fn drain(buf: &mut PlayoutBuffer<u16>, ticks: usize) -> Vec<Playout<u16>> {
        (0..ticks).map(|_| buf.tick()).collect()
    }

    use Playout::{Frame, Idle, Lost};

    #[test]
    fn waits_for_target_depth_then_plays_in_order() {
        let mut buf = buffer();
        buf.push(10, 10);
        assert_eq!(buf.tick(), Idle);
        buf.push(11, 11);
        assert_eq!(buf.tick(), Frame(10));
        buf.push(12, 12);
        assert_eq!(drain(&mut buf, 2), vec![Frame(11), Frame(12)]);
    }

    #[test]
    fn reorders_within_the_buffer() {
        let mut buf = buffer();
        buf.push(2, 2);
        buf.push(1, 1);
        buf.push(3, 3);
        assert_eq!(drain(&mut buf, 3), vec![Frame(1), Frame(2), Frame(3)]);
    }

    #[test]
    fn gap_with_later_frames_is_lost_and_skipped() {
        let mut buf = buffer();
        for seq in [1, 2, 4, 5] {
            buf.push(seq, seq);
        }
        assert_eq!(drain(&mut buf, 4), vec![Frame(1), Frame(2), Lost, Frame(4)]);
        assert_eq!(buf.stats().lost, 1);
    }

    #[test]
    fn late_frame_after_its_slot_is_dropped() {
        let mut buf = buffer();
        for seq in [1, 2, 4] {
            buf.push(seq, seq);
        }
        assert_eq!(drain(&mut buf, 3), vec![Frame(1), Frame(2), Lost]);
        buf.push(3, 3);
        assert_eq!(buf.stats().late, 1);
        assert_eq!(buf.tick(), Frame(4));
    }

    #[test]
    fn underrun_conceals_but_keeps_the_slot_for_a_late_frame() {
        let mut buf = buffer();
        buf.push(1, 1);
        buf.push(2, 2);
        assert_eq!(drain(&mut buf, 2), vec![Frame(1), Frame(2)]);
        // 3 is late, not lost
        assert_eq!(buf.tick(), Lost);
        buf.push(3, 3);
        assert_eq!(buf.tick(), Frame(3));
        assert_eq!(buf.stats().underruns, 1);
        assert_eq!(buf.stats().late, 0);
    }

    #[test]
    fn rebuffers_after_max_concealment() {
        let mut buf = buffer();
        buf.push(1, 1);
        buf.push(2, 2);
        drain(&mut buf, 2);
        let out = drain(&mut buf, 7);
        assert_eq!(out[..5], [Lost, Lost, Lost, Lost, Lost]);
        assert_eq!(out[5..], [Idle, Idle]);
        // the stream comes back: buffered to target before playing again
        buf.push(3, 3);
        assert_eq!(buf.tick(), Idle);
        buf.push(4, 4);
        assert_eq!(drain(&mut buf, 2), vec![Frame(3), Frame(4)]);
    }

    #[test]
    fn duplicates_are_ignored() {
        let mut buf = buffer();
        buf.push(1, 1);
        buf.push(1, 100);
        buf.push(2, 2);
        assert_eq!(drain(&mut buf, 2), vec![Frame(1), Frame(2)]);
        assert_eq!(buf.stats().duplicates, 1);
    }

    #[test]
    fn sequence_wraparound_keeps_order() {
        let mut buf = buffer();
        buf.push(65534, 65534);
        buf.push(65535, 65535);
        buf.push(1, 1);
        buf.push(0, 0);
        assert_eq!(
            drain(&mut buf, 4),
            vec![Frame(65534), Frame(65535), Frame(0), Frame(1)]
        );
    }

    #[test]
    fn overflow_drops_the_oldest() {
        let mut buf = buffer();
        for seq in 0..12 {
            buf.push(seq, seq);
        }
        assert_eq!(buf.len(), 10);
        assert_eq!(buf.stats().dropped, 2);
        assert_eq!(buf.tick(), Frame(2));
    }

    #[test]
    fn big_sequence_jump_restarts_the_stream() {
        let mut buf = buffer();
        buf.push(1, 1);
        buf.push(2, 2);
        assert_eq!(buf.tick(), Frame(1));
        buf.push(5000, 5000);
        assert_eq!(buf.stats().resets, 1);
        assert_eq!(buf.tick(), Idle);
        buf.push(5001, 5001);
        assert_eq!(drain(&mut buf, 2), vec![Frame(5000), Frame(5001)]);
    }

    #[test]
    fn new_ssrc_restarts_the_stream() {
        let mut buf = buffer();
        buf.set_source(1);
        buf.push(100, 100);
        buf.push(101, 101);
        assert_eq!(buf.tick(), Frame(100));
        // the new stream's numbers land just behind the old ones
        buf.set_source(2);
        buf.push(90, 90);
        buf.push(91, 91);
        assert_eq!(buf.stats().resets, 1);
        assert_eq!(buf.stats().late, 0);
        assert_eq!(drain(&mut buf, 2), vec![Frame(90), Frame(91)]);
        // the same source again changes nothing
        buf.set_source(2);
        assert_eq!(buf.stats().resets, 1);
    }

    #[test]
    fn delay_grown_by_underruns_is_trimmed() {
        let config = PlayoutConfig {
            trim_after_ticks: 3,
            ..Default::default()
        };
        let mut buf = PlayoutBuffer::new(config);
        let mut seq = 0u16;
        let mut push = |buf: &mut PlayoutBuffer<u16>, n: usize| {
            for _ in 0..n {
                buf.push(seq, seq);
                seq += 1;
            }
        };
        // a burst leaves 5 frames buffered on a steady one-in-one-out stream
        push(&mut buf, 6);
        let mut played = vec![];
        for _ in 0..3 {
            if let Frame(f) = buf.tick() {
                played.push(f);
            }
            push(&mut buf, 1);
        }
        assert_eq!(buf.stats().dropped, 1);
        assert!(buf.len() < 6, "delay not trimmed: {} buffered", buf.len());
        assert!(played.windows(2).all(|w| w[0] < w[1]));
    }
}
