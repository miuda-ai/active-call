use crate::{media::AudioFrame, media::PcmBuf, media::Samples};
use audio_codec::{
    CodecType, Decoder, Encoder, Resampler, bytes_to_samples,
    g722::{G722Decoder, G722Encoder},
    pcma::{PcmaDecoder, PcmaEncoder},
    pcmu::{PcmuDecoder, PcmuEncoder},
    samples_to_bytes,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use audio_codec::g729::{G729Decoder, G729Encoder};
#[cfg(feature = "opus")]
use audio_codec::opus::{OpusDecoder, OpusEncoder};

pub struct TrackCodec {
    pcmu_encoder: PcmuEncoder,
    pcmu_decoder: PcmuDecoder,
    pcma_encoder: PcmaEncoder,
    pcma_decoder: PcmaDecoder,

    g722_encoder: Option<Box<G722Encoder>>,
    g722_decoder: Option<Box<G722Decoder>>,

    g729_encoder: Option<Box<G729Encoder>>,
    g729_decoder: Option<Box<G729Decoder>>,

    #[cfg(feature = "opus")]
    opus_encoder: Option<OpusEncoder>,
    #[cfg(feature = "opus")]
    opus_decoder: Option<OpusDecoder>,

    resampler: Option<Resampler>,
    resampler_in_rate: u32,
    resampler_out_rate: u32,
    pub payload_type_map: Arc<RwLock<HashMap<u8, CodecType>>>,
}

impl Clone for TrackCodec {
    fn clone(&self) -> Self {
        let mut new = Self::new();
        // Share the same underlying map so reinvite PT updates are visible to all clones.
        new.payload_type_map = Arc::clone(&self.payload_type_map);
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
        #[cfg(feature = "opus")]
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
            #[cfg(feature = "opus")]
            opus_encoder: None,
            #[cfg(feature = "opus")]
            opus_decoder: None,
            resampler: None,
            resampler_in_rate: 0,
            resampler_out_rate: 0,
            payload_type_map,
        }
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

    pub fn is_audio(payload_type: u8) -> bool {
        match payload_type {
            0 | 8 | 9 | 18 | 111 => true,
            101 => false, // Telephone Event (DTMF) should not be decoded as audio
            pt if pt >= 96 && pt <= 127 => true,
            _ => false,
        }
    }

    pub fn decode(
        &mut self,
        payload_type: u8,
        payload: &[u8],
        target_sample_rate: u32,
    ) -> (u32, u16, PcmBuf) {
        let codec = self
            .payload_type_map
            .read()
            .unwrap()
            .get(&payload_type)
            .cloned()
            .unwrap_or_else(|| match payload_type {
                0 => CodecType::PCMU,
                8 => CodecType::PCMA,
                9 => CodecType::G722,
                18 => CodecType::G729,
                #[cfg(feature = "opus")]
                111 => CodecType::Opus,
                _ => CodecType::PCMU,
            });

        let pcm = match codec {
            CodecType::PCMU => self.pcmu_decoder.decode(payload),
            CodecType::PCMA => self.pcma_decoder.decode(payload),
            CodecType::G722 => self
                .g722_decoder
                .get_or_insert_with(|| Box::new(G722Decoder::new()))
                .decode(payload),
            CodecType::G729 => self
                .g729_decoder
                .get_or_insert_with(|| Box::new(G729Decoder::new()))
                .decode(payload),
            #[cfg(feature = "opus")]
            CodecType::Opus => self
                .opus_decoder
                .get_or_insert_with(OpusDecoder::new_default)
                .decode(payload),
            _ => bytes_to_samples(payload),
        };

        let (in_rate, channels) = match codec {
            CodecType::PCMU => (8000, 1),
            CodecType::PCMA => (8000, 1),
            CodecType::G722 => (16000, 1),
            CodecType::G729 => (8000, 1),
            #[cfg(feature = "opus")]
            CodecType::Opus => {
                if pcm.len() >= 1920 {
                    (48000, 2)
                } else {
                    (48000, 1)
                }
            }
            _ => (8000, 1),
        };

        (
            target_sample_rate,
            channels,
            self.resample(pcm, in_rate, target_sample_rate),
        )
    }

    pub fn resample(&mut self, pcm: PcmBuf, in_rate: u32, out_rate: u32) -> PcmBuf {
        if in_rate == out_rate {
            return pcm;
        }

        if self.resampler.is_none()
            || self.resampler_in_rate != in_rate
            || self.resampler_out_rate != out_rate
        {
            self.resampler = Some(Resampler::new(in_rate as usize, out_rate as usize));
            self.resampler_in_rate = in_rate;
            self.resampler_out_rate = out_rate;
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
                    if self.resampler.is_none()
                        || self.resampler_in_rate != frame.sample_rate
                        || self.resampler_out_rate != target_samplerate
                    {
                        self.resampler = Some(Resampler::new(
                            frame.sample_rate as usize,
                            target_samplerate as usize,
                        ));
                        self.resampler_in_rate = frame.sample_rate;
                        self.resampler_out_rate = target_samplerate;
                    }
                    pcm = self.resampler.as_mut().unwrap().resample(&pcm);
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
                    #[cfg(feature = "opus")]
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

    #[cfg(feature = "opus")]
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
}
