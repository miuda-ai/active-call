use crate::{media::AudioFrame, media::PcmBuf, media::Samples};
use audio_codec::{
    CodecType, Decoder, Encoder, Resampler, bytes_to_samples,
    g722::{G722Decoder, G722Encoder},
    pcma::{PcmaDecoder, PcmaEncoder},
    pcmu::{PcmuDecoder, PcmuEncoder},
    samples_to_bytes,
};
use std::collections::HashMap;

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
    pub payload_type_map: HashMap<u8, CodecType>,
}

impl Clone for TrackCodec {
    fn clone(&self) -> Self {
        let mut new = Self::new();
        new.payload_type_map = self.payload_type_map.clone();
        new
    }
}

impl TrackCodec {
    pub fn new() -> Self {
        let mut payload_type_map = HashMap::new();
        payload_type_map.insert(0, CodecType::PCMU);
        payload_type_map.insert(8, CodecType::PCMA);
        payload_type_map.insert(9, CodecType::G722);
        payload_type_map.insert(18, CodecType::G729);
        payload_type_map.insert(101, CodecType::TelephoneEvent);
        payload_type_map.insert(111, CodecType::Opus);

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
        self.payload_type_map.insert(pt, codec);
    }

    pub fn is_audio(payload_type: u8) -> bool {
        match payload_type {
            0 | 8 | 9 | 18 | 111 => true,
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
            .get(&payload_type)
            .cloned()
            .unwrap_or_else(|| match payload_type {
                0 => CodecType::PCMU,
                8 => CodecType::PCMA,
                9 => CodecType::G722,
                18 => CodecType::G729,
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
                let target_samplerate = match payload_type {
                    0 => 8000,
                    8 => 8000,
                    9 => 16000,
                    18 => 8000,
                    111 => 48000, // Opus sample rate
                    _ => 8000,
                };

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

                let payload = match payload_type {
                    0 => self.pcmu_encoder.encode(&pcm),
                    8 => self.pcma_encoder.encode(&pcm),
                    9 => self
                        .g722_encoder
                        .get_or_insert_with(|| Box::new(G722Encoder::new()))
                        .encode(&pcm),
                    18 => self
                        .g729_encoder
                        .get_or_insert_with(|| Box::new(G729Encoder::new()))
                        .encode(&pcm),
                    #[cfg(feature = "opus")]
                    111 => self
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
