use crate::media::PcmBuf;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    pub username: String,
    pub realm: String,
    pub source: String,
}

impl From<&String> for Attendee {
    fn from(source: &String) -> Self {
        let uri = rsip::Uri::try_from(source.as_str()).unwrap_or_default();
        Self {
            username: uri.user().unwrap_or_default().to_string(),
            realm: uri.host().to_string(),
            source: source.to_string(),
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    Incoming {
        track_id: String,
        timestamp: u64,
        caller: String,
        callee: String,
        sdp: String,
    },
    Answer {
        track_id: String,
        timestamp: u64,
        sdp: String,
        refer: Option<bool>,
    },
    Reject {
        track_id: String,
        timestamp: u64,
        reason: String,
        refer: Option<bool>,
        code: Option<u32>,
    },
    Ringing {
        track_id: String,
        timestamp: u64,
        early_media: bool,
        refer: Option<bool>,
    },
    Hangup {
        track_id: String,
        timestamp: u64,
        reason: Option<String>,
        initiator: Option<String>,
        start_time: String,
        hangup_time: String,
        answer_time: Option<String>,
        ringing_time: Option<String>,
        from: Option<Attendee>,
        to: Option<Attendee>,
        extra: Option<HashMap<String, serde_json::Value>>,
        refer: Option<bool>,
    },
    AnswerMachineDetection {
        // Answer machine detection
        track_id: String,
        timestamp: u64,
        start_time: u64,
        end_time: u64,
        text: String,
    },
    Transcription {
        track_id: String,
        text: String,
        is_final: bool,
        timestamp: u64,
        extra: Option<serde_json::Value>,
    },
    Interrupt {
        receiver: Option<String>,
    },
    FunctionCall {
        track_id: String,
        call_id: String,
        name: String,
        arguments: String,
        timestamp: u64,
    },
    Speaking {
        track_id: String,
        timestamp: u64,
        start_time: u64,
        is_filler: Option<bool>,
        confidence: Option<f32>,
    },
    Silence {
        track_id: String,
        timestamp: u64,
        start_time: u64,
        duration: u64,
        #[serde(skip)]
        samples: Option<PcmBuf>,
    },
    ///End of Utterance
    Eou {
        track_id: String,
        timestamp: u64,
        completed: bool,
    },
    ///Inactivity timeout
    Inactivity {
        track_id: String,
        timestamp: u64,
    },
    Dtmf {
        track_id: String,
        timestamp: u64,
        digit: String,
    },
    TrackStart {
        track_id: String,
        timestamp: u64,
        play_id: Option<String>,
    },
    TrackEnd {
        track_id: String,
        timestamp: u64,
        duration: u64,
        ssrc: u32,
        play_id: Option<String>,
    },
    Interruption {
        track_id: String,
        timestamp: u64,
        play_id: Option<String>,
        subtitle: Option<String>, // current tts text
        position: Option<u32>,    // word index in subtitle
        total_duration: u32,      // whole tts duration
        current: u32,             // elapsed time since start of tts
    },
    AsrFinal {
        track_id: String,
        timestamp: u64,
        index: u32,
        start_time: Option<u64>,
        end_time: Option<u64>,
        text: String,
        is_filler: Option<bool>,
        confidence: Option<f32>,
    },
    AsrDelta {
        track_id: String,
        index: u32,
        timestamp: u64,
        start_time: Option<u64>,
        end_time: Option<u64>,
        text: String,
        is_filler: Option<bool>,
        confidence: Option<f32>,
    },
    Metrics {
        timestamp: u64,
        key: String,
        duration: u32,
        data: serde_json::Value,
    },
    Error {
        track_id: String,
        timestamp: u64,
        sender: String,
        error: String,
        code: Option<u32>,
    },
    AddHistory {
        sender: Option<String>,
        timestamp: u64,
        speaker: String,
        text: String,
    },
    Other {
        track_id: String,
        timestamp: u64,
        sender: String,
        extra: Option<HashMap<String, String>>,
    },
    Binary {
        track_id: String,
        timestamp: u64,
        data: Vec<u8>,
    },
    Ping {
        timestamp: u64,
        payload: Option<String>,
    },
}

pub type EventSender = tokio::sync::broadcast::Sender<SessionEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<SessionEvent>;

pub fn create_event_sender() -> EventSender {
    EventSender::new(128)
}
