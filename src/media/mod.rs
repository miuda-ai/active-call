use serde::{Deserialize, Serialize};

pub mod ambiance;
pub mod asr_processor;
pub mod cache;
pub mod denoiser;
pub mod dtmf;
pub mod engine;
pub mod inactivity;
pub mod loader;
pub mod negotiate;
pub mod processor;
pub mod realtime_processor;
pub mod recorder;
pub mod stream;
#[cfg(test)]
mod tests;
pub mod track;
pub mod vad;
pub mod volume_control;
pub use audio_codec::PcmBuf;
pub use audio_codec::Sample;
pub const INTERNAL_SAMPLERATE: u32 = 16000;
pub type TrackId = String;
pub type PayloadBuf = Vec<u8>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Samples {
    PCM {
        samples: PcmBuf,
    },
    RTP {
        sequence_number: u16,
        payload_type: u8,
        payload: PayloadBuf,
    },
    Empty,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFrame {
    pub track_id: TrackId,
    pub samples: Samples,
    pub timestamp: u64,
    pub sample_rate: u32,
    pub channels: u16,
}

impl Samples {
    pub fn payload_type(&self) -> Option<u8> {
        match self {
            Samples::RTP { payload_type, .. } => Some(*payload_type),
            _ => None,
        }
    }
}
// get timestamp in milliseconds
pub fn get_timestamp() -> u64 {
    let now = std::time::SystemTime::now();
    now.duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}
