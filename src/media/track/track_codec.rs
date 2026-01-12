use crate::{media::AudioFrame, media::PcmBuf, media::Samples};
use audio_codec::{
    CodecType, Decoder, Encoder, Resampler, bytes_to_samples,
    g722::{G722Decoder, G722Encoder},
    pcma::{PcmaDecoder, PcmaEncoder},
    pcmu::{PcmuDecoder, PcmuEncoder},
    samples_to_bytes,
};
use std::cell::RefCell;
use std::collections::HashMap;

use audio_codec::g729::{G729Decoder, G729Encoder};
#[cfg(feature = "opus")]
use audio_codec::opus::{OpusDecoder, OpusEncoder};

pub struct TrackCodec {
    pub pcmu_encoder: RefCell<PcmuEncoder>,
    pub pcmu_decoder: RefCell<PcmuDecoder>,
    pub pcma_encoder: RefCell<PcmaEncoder>,
    pub pcma_decoder: RefCell<PcmaDecoder>,

    pub g722_encoder: RefCell<G722Encoder>,
    pub g722_decoder: RefCell<G722Decoder>,

    pub g729_encoder: RefCell<G729Encoder>,
    pub g729_decoder: RefCell<G729Decoder>,

    #[cfg(feature = "opus")]
    pub opus_encoder: RefCell<Option<OpusEncoder>>,
    #[cfg(feature = "opus")]
    pub opus_decoder: RefCell<Option<OpusDecoder>>,

    pub resampler: RefCell<Option<Resampler>>,
    pub resampler_in_rate: RefCell<u32>,
    pub resampler_out_rate: RefCell<u32>,
    pub payload_type_map: RefCell<HashMap<u8, CodecType>>,
}
unsafe impl Send for TrackCodec {}
unsafe impl Sync for TrackCodec {}

impl Clone for TrackCodec {
    fn clone(&self) -> Self {
        let new = Self::new();
        *new.payload_type_map.borrow_mut() = self.payload_type_map.borrow().clone();
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
            pcmu_encoder: RefCell::new(PcmuEncoder::new()),
            pcmu_decoder: RefCell::new(PcmuDecoder::new()),
            pcma_encoder: RefCell::new(PcmaEncoder::new()),
            pcma_decoder: RefCell::new(PcmaDecoder::new()),
            g722_encoder: RefCell::new(G722Encoder::new()),
            g722_decoder: RefCell::new(G722Decoder::new()),
            g729_encoder: RefCell::new(G729Encoder::new()),
            g729_decoder: RefCell::new(G729Decoder::new()),
            #[cfg(feature = "opus")]
            opus_encoder: RefCell::new(None),
            #[cfg(feature = "opus")]
            opus_decoder: RefCell::new(None),
            resampler: RefCell::new(None),
            resampler_in_rate: RefCell::new(0),
            resampler_out_rate: RefCell::new(0),
            payload_type_map: RefCell::new(payload_type_map),
        }
    }

    pub fn set_payload_type(&self, pt: u8, codec: CodecType) {
        self.payload_type_map.borrow_mut().insert(pt, codec);
    }

    pub fn is_audio(payload_type: u8) -> bool {
        match payload_type {
            0 | 8 | 9 | 18 | 111 => true,
            pt if pt >= 96 && pt <= 127 => true,
            _ => false,
        }
    }

    pub fn decode(
        &self,
        payload_type: u8,
        payload: &[u8],
        channels: u16,
        target_sample_rate: u32,
    ) -> PcmBuf {
        let codec = self
            .payload_type_map
            .borrow()
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

        let mut pcm = match codec {
            CodecType::PCMU => self.pcmu_decoder.borrow_mut().decode(payload),
            CodecType::PCMA => self.pcma_decoder.borrow_mut().decode(payload),
            CodecType::G722 => self.g722_decoder.borrow_mut().decode(payload),
            CodecType::G729 => self.g729_decoder.borrow_mut().decode(payload),
            #[cfg(feature = "opus")]
            CodecType::Opus => {
                let mut opus_decoder = self.opus_decoder.borrow_mut();
                if opus_decoder.is_none() {
                    *opus_decoder = Some(OpusDecoder::new_default());
                }
                if let Some(ref mut decoder) = opus_decoder.as_mut() {
                    decoder.decode(payload)
                } else {
                    bytes_to_samples(payload)
                }
            }
            _ => bytes_to_samples(payload),
        };

        if channels == 2 {
            let mut mono = Vec::with_capacity(pcm.len() / 2);
            for i in (0..pcm.len()).step_by(2) {
                if i + 1 < pcm.len() {
                    let mixed = ((pcm[i] as i32 + pcm[i + 1] as i32) / 2) as i16;
                    mono.push(mixed);
                } else {
                    mono.push(pcm[i]);
                }
            }
            pcm = mono;
        }

        let sample_rate = match codec {
            CodecType::PCMU => 8000,
            CodecType::PCMA => 8000,
            CodecType::G722 => 16000,
            CodecType::G729 => 8000,
            #[cfg(feature = "opus")]
            CodecType::Opus => 48000,
            _ => 8000,
        };

        if sample_rate != target_sample_rate {
            if self.resampler.borrow().is_none()
                || *self.resampler_in_rate.borrow() != sample_rate
                || *self.resampler_out_rate.borrow() != target_sample_rate
            {
                self.resampler.borrow_mut().replace(Resampler::new(
                    sample_rate as usize,
                    target_sample_rate as usize,
                ));
                *self.resampler_in_rate.borrow_mut() = sample_rate;
                *self.resampler_out_rate.borrow_mut() = target_sample_rate;
            }
            self.resampler.borrow_mut().as_mut().unwrap().resample(&pcm)
        } else {
            pcm
        }
    }

    pub fn encode(&self, payload_type: u8, frame: AudioFrame) -> (u8, Vec<u8>) {
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
                    if self.resampler.borrow().is_none()
                        || *self.resampler_in_rate.borrow() != frame.sample_rate
                        || *self.resampler_out_rate.borrow() != target_samplerate
                    {
                        self.resampler.borrow_mut().replace(Resampler::new(
                            frame.sample_rate as usize,
                            target_samplerate as usize,
                        ));
                        *self.resampler_in_rate.borrow_mut() = frame.sample_rate;
                        *self.resampler_out_rate.borrow_mut() = target_samplerate;
                    }
                    pcm = self.resampler.borrow_mut().as_mut().unwrap().resample(&pcm);
                }

                let payload = match payload_type {
                    0 => self.pcmu_encoder.borrow_mut().encode(&pcm),
                    8 => self.pcma_encoder.borrow_mut().encode(&pcm),
                    9 => self.g722_encoder.borrow_mut().encode(&pcm),
                    18 => self.g729_encoder.borrow_mut().encode(&pcm),
                    #[cfg(feature = "opus")]
                    111 => {
                        let mut opus_encoder = self.opus_encoder.borrow_mut();
                        if opus_encoder.is_none() {
                            *opus_encoder = Some(OpusEncoder::new_default());
                        }
                        if let Some(ref mut encoder) = opus_encoder.as_mut() {
                            encoder.encode(&pcm)
                        } else {
                            samples_to_bytes(&pcm)
                        }
                    }
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
