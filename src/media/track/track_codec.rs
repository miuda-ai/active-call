use crate::{media::AudioFrame, media::PcmBuf, media::Samples};
use audio_codec::{
    BoxedResampler, CodecType, Decoder, Encoder, bytes_to_samples,
    g722::{G722Decoder, G722Encoder},
    pcma::{PcmaDecoder, PcmaEncoder},
    pcmu::{PcmuDecoder, PcmuEncoder},
    samples_to_bytes,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::warn;

use audio_codec::g729::{G729Decoder, G729Encoder};
use audio_codec::opus::{OpusDecoder, OpusEncoder};

/// Convert an audio duration to RTP clock ticks.
pub fn duration_to_rtp_ticks(duration: Duration, clock_rate: u32) -> u32 {
    (duration.as_nanos() * clock_rate as u128 / 1_000_000_000) as u32
}

pub struct TrackCodec {
    pcmu_encoder: PcmuEncoder,
    pcmu_decoder: PcmuDecoder,
    pcma_encoder: PcmaEncoder,
    pcma_decoder: PcmaDecoder,

    g722_encoder: Option<Box<G722Encoder>>,
    g722_decoder: Option<Box<G722Decoder>>,

    g729_encoder: Option<Box<G729Encoder>>,
    g729_decoder: Option<Box<G729Decoder>>,

    opus_encoder: Option<OpusEncoder>,
    opus_decoder: Option<OpusDecoder>,

    resampler: Option<BoxedResampler>,
    resampler_in_rate: u32,
    resampler_out_rate: u32,
    pub payload_type_map: Arc<RwLock<HashMap<u8, CodecType>>>,
    /// Decoders conceal lost packets (see `conceal`); off by default, as it
    /// costs a history copy per decoded packet for G.711 and G.722.
    concealment: bool,
    /// Payload type of the last packet decoded, whose decoder conceals.
    last_payload_type: Option<u8>,
}

impl Clone for TrackCodec {
    fn clone(&self) -> Self {
        let mut new = Self::new();
        // Share the same underlying map so reinvite PT updates are visible to all clones.
        new.payload_type_map = Arc::clone(&self.payload_type_map);
        if self.concealment {
            new.enable_concealment();
        }
        new
    }
}

impl TrackCodec {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert(0, CodecType::PCMU);
        map.insert(8, CodecType::PCMA);
        map.insert(9, CodecType::G722);
        map.insert(18, CodecType::G729);
        map.insert(101, CodecType::TelephoneEvent);
        map.insert(111, CodecType::Opus);
        let payload_type_map = Arc::new(RwLock::new(map));

        Self {
            pcmu_encoder: PcmuEncoder::new(),
            pcmu_decoder: PcmuDecoder::new(),
            pcma_encoder: PcmaEncoder::new(),
            pcma_decoder: PcmaDecoder::new(),
            g722_encoder: None,
            g722_decoder: None,
            g729_encoder: None,
            g729_decoder: None,
            opus_encoder: None,
            opus_decoder: None,
            resampler: None,
            resampler_in_rate: 0,
            resampler_out_rate: 0,
            payload_type_map,
            concealment: false,
            last_payload_type: None,
        }
    }

    /// Have the decoders conceal lost packets, for [`Self::conceal`].
    pub fn enable_concealment(&mut self) {
        self.concealment = true;
        self.pcmu_decoder.enable_concealment();
        self.pcma_decoder.enable_concealment();
        for decoder in [
            self.g722_decoder
                .as_deref_mut()
                .map(|d| d as &mut dyn Decoder),
            self.g729_decoder
                .as_deref_mut()
                .map(|d| d as &mut dyn Decoder),
            self.opus_decoder.as_mut().map(|d| d as &mut dyn Decoder),
        ]
        .into_iter()
        .flatten()
        {
            decoder.enable_concealment();
        }
    }

    fn codec_for_pt(&self, payload_type: u8) -> CodecType {
        self.payload_type_map
            .read()
            .unwrap()
            .get(&payload_type)
            .cloned()
            .unwrap_or_else(|| match payload_type {
                0 => CodecType::PCMU,
                8 => CodecType::PCMA,
                9 => CodecType::G722,
                18 => CodecType::G729,
                111 => CodecType::Opus,
                _ => CodecType::PCMU,
            })
    }

    /// The decoder for an audio codec, created on first use.
    fn decoder(&mut self, codec: CodecType) -> Option<&mut dyn Decoder> {
        let concealment = self.concealment;
        let decoder: &mut dyn Decoder = match codec {
            CodecType::PCMU => &mut self.pcmu_decoder,
            CodecType::PCMA => &mut self.pcma_decoder,
            CodecType::G722 => self
                .g722_decoder
                .get_or_insert_with(|| {
                    let mut d = Box::new(G722Decoder::new());
                    if concealment {
                        d.enable_concealment();
                    }
                    d
                })
                .as_mut(),
            CodecType::G729 => self
                .g729_decoder
                .get_or_insert_with(|| {
                    let mut d = Box::new(G729Decoder::new());
                    if concealment {
                        d.enable_concealment();
                    }
                    d
                })
                .as_mut(),
            CodecType::Opus => self.opus_decoder.get_or_insert_with(|| {
                let mut d = OpusDecoder::new_default();
                if concealment {
                    d.enable_concealment();
                }
                d
            }),
            _ => return None,
        };
        Some(decoder)
    }

    /// Native `(sample_rate, channels)` of decoded audio, given the decoded
    /// sample count for Opus, whose channel count follows the packets.
    fn native_format(codec: CodecType, samples: usize) -> (u32, u16) {
        match codec {
            CodecType::PCMU | CodecType::PCMA | CodecType::G729 => (8000, 1),
            CodecType::G722 => (16000, 1),
            CodecType::Opus if samples >= 1920 => (48000, 2),
            CodecType::Opus => (48000, 1),
            _ => (8000, 1),
        }
    }

    /// Audio for one lost packet of `ptime`, from the decoder of the last
    /// packet decoded, as `(native_sample_rate, channels, samples)` like
    /// [`Self::decode`], and whether `next` was used. With `next`, the
    /// payload type and payload of the packet right after the loss, Opus
    /// rebuilds the lost one from its in-band FEC (where the sender coded
    /// any, else it conceals); other codecs conceal. `None` when concealment
    /// is off, nothing was decoded yet, or the codec has no concealment.
    pub fn conceal(
        &mut self,
        ptime: Duration,
        next: Option<(u8, &[u8])>,
    ) -> Option<(u32, u16, PcmBuf, bool)> {
        if !self.concealment {
            return None;
        }
        let payload_type = self.last_payload_type?;
        let codec = self.codec_for_pt(payload_type);
        let (rate, _) = Self::native_format(codec, 0);
        let samples = (rate as u128 * ptime.as_nanos() / 1_000_000_000) as usize;
        let next = next
            .filter(|(pt, _)| *pt == payload_type && codec == CodecType::Opus)
            .map(|(_, p)| p);
        let used_next = next.is_some();
        let decoder = self.decoder(codec)?;
        let pcm = match next {
            Some(next) => decoder.recover(next, samples),
            None => decoder.conceal(samples),
        };
        if pcm.is_empty() {
            return None;
        }
        let (rate, channels) = Self::native_format(codec, pcm.len());
        Some((rate, channels, pcm, used_next))
    }

    pub fn set_payload_type(&mut self, pt: u8, codec: CodecType) {
        self.payload_type_map.write().unwrap().insert(pt, codec);
    }

    /// Look up the codec for a given RTP payload type, consulting the negotiated map first
    /// and falling back to the static payload type registry.
    pub fn get_codec_for_pt(&self, pt: u8) -> Option<CodecType> {
        self.payload_type_map
            .read()
            .unwrap()
            .get(&pt)
            .cloned()
            .or_else(|| CodecType::try_from(pt).ok())
    }

    /// Decode an RTP payload into PCM at the codec's native sample rate.
    /// Returns `(native_sample_rate, channels, samples)`; resampling to the
    /// pipeline rate is the caller's responsibility (see ProcessorChain).
    pub fn decode(&mut self, payload_type: u8, payload: &[u8]) -> (u32, u16, PcmBuf) {
        let codec = self.codec_for_pt(payload_type);
        let pcm = match self.decoder(codec) {
            Some(decoder) => {
                let pcm = decoder.decode(payload);
                self.last_payload_type = Some(payload_type);
                pcm
            }
            None => bytes_to_samples(payload),
        };
        let (in_rate, channels) = Self::native_format(codec, pcm.len());
        (in_rate, channels, pcm)
    }

    pub fn resample(&mut self, pcm: PcmBuf, in_rate: u32, out_rate: u32) -> PcmBuf {
        // A zero rate means the caller has no valid source rate (e.g. a
        // media-pass track created without an explicit input sample rate).
        // There is nothing sensible to resample from, so pass the samples
        // through instead of panicking: `BoxedResampler::new` rejects zero
        // rates, and unwinding here poisons the `ProcessorChain` mutex,
        // permanently breaking every subsequent frame on the track.
        if in_rate == 0 || out_rate == 0 || in_rate == out_rate {
            return pcm;
        }

        if self.resampler.is_none()
            || self.resampler_in_rate != in_rate
            || self.resampler_out_rate != out_rate
        {
            match BoxedResampler::new(in_rate as usize, out_rate as usize) {
                Ok(resampler) => {
                    self.resampler = Some(resampler);
                    self.resampler_in_rate = in_rate;
                    self.resampler_out_rate = out_rate;
                }
                Err(e) => {
                    warn!(in_rate, out_rate, "failed to build resampler: {e}");
                    self.resampler = None;
                    self.resampler_in_rate = 0;
                    self.resampler_out_rate = 0;
                    return pcm;
                }
            }
        }
        self.resampler.as_mut().unwrap().resample(&pcm)
    }

    pub fn encode(&mut self, payload_type: u8, frame: AudioFrame) -> (u8, Vec<u8>) {
        match frame.samples {
            Samples::PCM { samples: mut pcm } => {
                let codec = self
                    .payload_type_map
                    .read()
                    .unwrap()
                    .get(&payload_type)
                    .cloned()
                    .or_else(|| CodecType::try_from(payload_type).ok());

                let target_samplerate = codec.map(|c| c.samplerate()).unwrap_or(8000);
                if frame.sample_rate != target_samplerate {
                    pcm = self.resample(pcm, frame.sample_rate, target_samplerate);
                }

                let payload = match codec {
                    Some(CodecType::PCMU) => self.pcmu_encoder.encode(&pcm),
                    Some(CodecType::PCMA) => self.pcma_encoder.encode(&pcm),
                    Some(CodecType::G722) => self
                        .g722_encoder
                        .get_or_insert_with(|| Box::new(G722Encoder::new()))
                        .encode(&pcm),
                    Some(CodecType::G729) => self
                        .g729_encoder
                        .get_or_insert_with(|| Box::new(G729Encoder::new()))
                        .encode(&pcm),
                    Some(CodecType::Opus) => self
                        .opus_encoder
                        .get_or_insert_with(OpusEncoder::new_default)
                        .encode(&pcm),
                    _ => samples_to_bytes(&pcm),
                };
                (payload_type, payload)
            }
            Samples::RTP {
                payload_type,
                payload,
                ..
            } => (payload_type, payload),
            _ => (payload_type, vec![]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_dynamic_opus_payload_type_uses_opus_encoder() {
        let mut codec = TrackCodec::new();
        codec.set_payload_type(96, CodecType::Opus);

        let frame = AudioFrame {
            samples: Samples::PCM {
                // 20ms @ 16k mono
                samples: vec![0; 320],
            },
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        };

        let (pt, payload) = codec.encode(96, frame);
        assert_eq!(pt, 96);
        assert!(!payload.is_empty());
        // If this were raw PCM bytes, it would be 640 bytes.
        assert!(payload.len() < 640);
    }

    #[test]
    fn test_resample_zero_rate_passes_through_without_panic() {
        let mut codec = TrackCodec::new();
        let samples = vec![1000i16; 160];

        assert_eq!(codec.resample(samples.clone(), 0, 16000), samples);
        assert_eq!(codec.resample(samples.clone(), 16000, 0), samples);
        assert_eq!(codec.resample(samples.clone(), 0, 0), samples);
    }
}

#[cfg(test)]
mod conceal_tests {
    use super::*;
    use audio_codec::Encoder;

    const PTIME: Duration = Duration::from_millis(20);

    /// Speech-like: 8 harmonics of a pitch gliding up, under an envelope.
    fn voice(rate: u32, frames: usize) -> Vec<Vec<i16>> {
        let frame = (rate / 50) as usize;
        let mut phase = 0f32;
        let pcm: Vec<i16> = (0..frames * frame)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let f0 = 120.0 + 60.0 * (t / 0.6).min(1.0);
                phase += 2.0 * std::f32::consts::PI * f0 / rate as f32;
                let env = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 2.0 * t).sin();
                let v: f32 = (1..=8).map(|k| (k as f32 * phase).sin() / k as f32).sum();
                (6000.0 * env * v) as i16
            })
            .collect();
        pcm.chunks(frame).map(<[i16]>::to_vec).collect()
    }

    #[test]
    fn nothing_to_conceal_with_until_enabled_and_decoded() {
        let mut codec = TrackCodec::new();
        let payload = PcmaEncoder::new().encode(&voice(8000, 1)[0]);
        codec.decode(8, &payload);
        assert!(codec.conceal(PTIME, None).is_none(), "concealment is off");
        let mut codec = TrackCodec::new();
        codec.enable_concealment();
        assert!(codec.conceal(PTIME, None).is_none(), "nothing decoded yet");
    }

    #[test]
    fn pcma_conceals_in_its_native_format() {
        let mut codec = TrackCodec::new();
        codec.enable_concealment();
        let mut enc = PcmaEncoder::new();
        for frame in voice(8000, 6) {
            codec.decode(8, &enc.encode(&frame));
        }
        let (rate, channels, pcm, used_next) = codec.conceal(PTIME, None).unwrap();
        assert_eq!(
            (rate, channels, pcm.len(), used_next),
            (8000, 1, 160, false)
        );
        assert!(pcm.iter().any(|s| *s != 0));
    }

    #[test]
    fn opus_rebuilds_a_lost_packet_from_the_next_ones_fec() {
        let mut enc = OpusEncoder::new(48000, 1);
        enc.set_bitrate(32000);
        enc.set_inband_fec(true);
        enc.set_packet_loss_perc(20);
        let packets: Vec<Vec<u8>> = voice(48000, 30).iter().map(|f| enc.encode(f)).collect();
        let mut reference = TrackCodec::new();
        let clean: Vec<PcmBuf> = packets.iter().map(|p| reference.decode(111, p).2).collect();

        let error_db = |out: &[i16], clean: &[i16]| {
            let err: f64 = out
                .iter()
                .zip(clean)
                .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
                .sum();
            let sig: f64 = clean.iter().map(|b| (*b as f64).powi(2)).sum();
            10.0 * (err / sig).log10()
        };
        // the encoder codes FEC frame by frame (here not into the quietest
        // stretch, frames 14-21): lose frames it protected
        let losses = [6, 9, 12, 24];
        let (mut fec_db, mut plc_db) = (0f64, 0f64);
        for lost in losses {
            let mut recovering = TrackCodec::new();
            let mut concealing = TrackCodec::new();
            for codec in [&mut recovering, &mut concealing] {
                codec.enable_concealment();
                for p in &packets[..lost] {
                    codec.decode(111, p);
                }
            }
            let (rate, _, recovered, used_next) = recovering
                .conceal(PTIME, Some((111, &packets[lost + 1])))
                .unwrap();
            assert_eq!((rate, recovered.len(), used_next), (48000, 960, true));
            let (_, _, concealed, used_next) = concealing.conceal(PTIME, None).unwrap();
            assert!(!used_next);
            fec_db += error_db(&recovered, &clean[lost]);
            plc_db += error_db(&concealed, &clean[lost]);
        }
        let (fec_db, plc_db) = (fec_db / losses.len() as f64, plc_db / losses.len() as f64);
        // measured: FEC -29.8 dB, concealment -7.5 dB
        assert!(
            fec_db < plc_db - 10.0,
            "FEC {fec_db:.1} dB vs concealment {plc_db:.1} dB"
        );
        let mut recovering = TrackCodec::new();
        recovering.enable_concealment();
        recovering.decode(111, &packets[0]);
        // a next packet of another payload type is no use
        let (_, _, _, used_next) = recovering.conceal(PTIME, Some((0, &packets[1]))).unwrap();
        assert!(!used_next);
    }
}

#[cfg(test)]
mod duration_tests {
    use super::*;

    #[test]
    fn test_duration_to_rtp_ticks() {
        assert_eq!(duration_to_rtp_ticks(Duration::from_millis(20), 8000), 160);
        assert_eq!(duration_to_rtp_ticks(Duration::from_millis(20), 48000), 960);
        assert_eq!(
            duration_to_rtp_ticks(Duration::from_micros(2_500), 48000),
            120
        );
    }
}
