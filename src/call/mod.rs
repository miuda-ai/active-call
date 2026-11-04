use crate::{CallOption, ReferOption, media::recorder::RecorderOption, synthesis::SynthesisOption};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub mod active_call;
pub mod refer_headers;
pub mod sip;
pub mod state;
pub mod tracks;
pub use active_call::ActiveCall;
pub use active_call::ActiveCallRef;
pub use active_call::ActiveCallType;
pub use refer_headers::{PassThroughReferHeaders, ReferHeaderContext, ReferHeaderResolver};

pub type CommandSender = tokio::sync::broadcast::Sender<Command>;
pub type CommandReceiver = tokio::sync::broadcast::Receiver<Command>;

// WebSocket Commands
#[skip_serializing_none]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(
    tag = "command",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    Invite {
        option: CallOption,
    },
    Accept {
        option: CallOption,
    },
    Reject {
        reason: String,
        code: Option<u32>,
    },
    Ringing {
        recorder: Option<RecorderOption>,
        early_media: Option<bool>,
        ringtone: Option<String>,
    },
    Tts {
        text: String,
        speaker: Option<String>,
        /// If the play_id is the same, it will not interrupt the previous playback
        play_id: Option<String>,
        /// If auto_hangup is true, it means the call will be hung up automatically after the TTS playback is finished
        auto_hangup: Option<bool>,
        /// If streaming is true, it means the input text is streaming text,
        /// and end_of_stream needs to be used to determine if it's finished,
        /// equivalent to LLM's streaming output to TTS synthesis
        streaming: Option<bool>,
        /// If end_of_stream is true, it means the input text is finished
        end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
        wait_input_timeout: Option<u32>,
        /// if true, the text is base64 encoded pcm samples
        base64: Option<bool>,
        /// Customizing cache key for TTS Result
        cache_key: Option<String>,
    },
    Play {
        url: String,
        play_id: Option<String>,
        auto_hangup: Option<bool>,
        wait_input_timeout: Option<u32>,
        offset_ms: Option<u32>,
    },
    /// Send one DTMF digit (0-9, *, #, A-D) to the remote party. RTP legs
    /// emit an RFC 4733 telephone-event on the negotiated payload type;
    /// outgoing audio is muted while the digit is on the wire. Runs on its
    /// own track, so it neither interrupts playback nor is interrupted by it.
    Dtmf {
        digit: char,
        /// Duration of the digit, default 100ms.
        duration_ms: Option<u32>,
        play_id: Option<String>,
    },
    Interrupt {
        graceful: Option<bool>,
        fade_out_ms: Option<u32>,
    },
    Pause {},
    Resume {},
    Hangup {
        reason: Option<String>,
        initiator: Option<String>,
        headers: Option<HashMap<String, String>>,
        refer: Option<bool>,
    },
    Refer {
        caller: String,
        /// aor of the calee, e.g., sip:bob@restsend.com
        callee: String,
        options: Option<ReferOption>,
    },
    Message {
        /// MIME body to send in a SIP MESSAGE request.
        body: String,
        /// Defaults to text/plain;charset=utf-8.
        content_type: Option<String>,
        /// Additional SIP headers for the MESSAGE request.
        headers: Option<HashMap<String, String>>,
        /// If true, send on the active refer dialog instead of the main call dialog.
        refer: Option<bool>,
    },
    /// Bridge audio with another established call.
    /// This creates separate bridge tracks for the two sessions and patches
    /// audio bidirectionally. It does not replace the server-side track and
    /// does not control hangup; each call keeps its own session/event flow.
    Bridge {
        /// session_id of the other call to bridge audio with
        target_session_id: String,
    },
    /// Remove audio bridge tracks with another established call.
    Unbridge {
        /// session_id of the other call to unbridge from
        target_session_id: String,
    },
    Mute {
        track_id: Option<String>,
    },
    Unmute {
        track_id: Option<String>,
    },
    History {
        speaker: String,
        text: String,
    },
    Custom {
        sender: Option<String>,
        data: serde_json::Value,
    },
    /// Trickle ICE: feed a remote candidate into an already-established session.
    AddIceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u32>,
    },
    /// Apply a new remote offer to the already-established WebRTC track
    /// (e.g. an ICE restart after the caller's network changed). The new
    /// answer is delivered as an `answer` event.
    Renegotiate {
        offer: String,
    },
}

/// Routing state for managing stateful load balancing
#[derive(Debug)]
pub struct RoutingState {
    /// Round-robin counters for each destination group
    round_robin_counters: Arc<Mutex<HashMap<String, usize>>>,
}

impl Default for RoutingState {
    fn default() -> Self {
        Self::new()
    }
}

impl RoutingState {
    pub fn new() -> Self {
        Self {
            round_robin_counters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get the next trunk index for round-robin selection
    pub fn next_round_robin_index(&self, destination_key: &str, trunk_count: usize) -> usize {
        if trunk_count == 0 {
            return 0;
        }

        let mut counters = self.round_robin_counters.lock().unwrap();
        let counter = counters
            .entry(destination_key.to_string())
            .or_insert_with(|| 0);
        let r = *counter % trunk_count;
        *counter += 1;
        return r;
    }
}

#[cfg(test)]
mod tests {
    use super::Command;

    #[test]
    fn diagnostic_commands_omit_sip_password() {
        assert!(format!("{:?}", crate::SipOption::default()).contains("password: None"));
        let sip = serde_json::json!({"username": "alice", "password": "dummy-sip-secret"});
        for value in [
            serde_json::json!({"command": "invite", "option": {"sip": sip}}),
            serde_json::json!({"command": "accept", "option": {"sip": sip}}),
            serde_json::json!({"command": "refer", "caller": "a", "callee": "b", "options": {"sip": sip}}),
        ] {
            let command: Command = serde_json::from_value(value).unwrap();
            let debug = format!("{command:?}");
            assert!(debug.contains("alice"));
            assert!(debug.contains(r#"password: Some("***")"#));
            assert!(!debug.contains("dummy-sip-secret"));
            let serialized = serde_json::to_value(&command).unwrap();
            let sip = match command {
                Command::Invite { option } | Command::Accept { option } => option.sip.unwrap(),
                Command::Refer { options, .. } => options.unwrap().sip.unwrap(),
                _ => unreachable!(),
            };
            assert_eq!(sip.password.as_deref(), Some("dummy-sip-secret"));
            let options = serialized.get("option").or_else(|| serialized.get("options")).unwrap();
            assert!(options["sip"].get("password").is_none());
            assert_eq!(options["sip"]["username"], "alice");
        }
    }

    #[test]
    fn message_command_deserializes_body() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "command": "message",
            "body": "customer_id=12345",
            "contentType": "text/plain"
        }))
        .unwrap();

        assert!(matches!(
            command,
            Command::Message {
                body,
                content_type: Some(content_type),
                ..
            } if body == "customer_id=12345" && content_type == "text/plain"
        ));
    }

    #[test]
    fn dtmf_command_deserializes() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "command": "dtmf",
            "digit": "#",
            "durationMs": 80,
            "playId": "notify"
        }))
        .unwrap();

        assert!(matches!(
            command,
            Command::Dtmf {
                digit: '#',
                duration_ms: Some(80),
                play_id: Some(play_id),
            } if play_id == "notify"
        ));

        // Exactly one digit.
        assert!(
            serde_json::from_value::<Command>(serde_json::json!({
                "command": "dtmf",
                "digit": "12"
            }))
            .is_err()
        );
    }

    #[test]
    fn message_command_deserializes_legacy_text() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "command": "message",
            "body": "customer_id=12345"
        }))
        .unwrap();

        assert!(matches!(
            command,
            Command::Message { body, .. } if body == "customer_id=12345"
        ));
    }

    #[test]
    fn message_command_serializes_body() {
        let command = Command::Message {
            body: "customer_id=12345".to_string(),
            content_type: None,
            headers: None,
            refer: None,
        };
        let value = serde_json::to_value(command).unwrap();

        assert_eq!(value["body"], "customer_id=12345");
        assert!(value.get("text").is_none());
    }
}
