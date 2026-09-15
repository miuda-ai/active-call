use crate::CallOption;
use crate::app::AppState;
use crate::call::active_call::ActiveCallType;
use crate::callrecord::{CallRecord, CallRecordHangupReason};
use crate::event::SessionEvent;
use crate::media::TrackId;
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use rsipstack::dialog::dialog::TerminatedReason;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub type Extras = Arc<ArcSwap<HashMap<String, Value>>>;

#[derive(Clone, Debug, Default)]
pub struct CallProgress {
    pub session_id: String,
    pub start_time: Option<DateTime<Utc>>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub answer: Option<String>,
    pub last_status_code: u16,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub option: Option<CallOption>,
}

impl CallProgress {
    pub fn on_early(&mut self, code: u16) {
        self.ring_time.get_or_insert_with(Utc::now);
        self.last_status_code = code;
    }

    pub fn on_confirmed(&mut self, session_id: String) {
        self.session_id = session_id;
        self.answer_time.get_or_insert_with(Utc::now);
        self.last_status_code = 200;
    }

    pub fn on_answered(&mut self) {
        self.answer_time.get_or_insert_with(Utc::now);
        self.last_status_code = 200;
    }

    pub fn try_set_answer(&mut self, sdp: &str) {
        if self.answer.is_none() {
            self.answer = Some(sdp.to_string());
        }
    }

    pub fn set_hangup_reason(&mut self, reason: CallRecordHangupReason) {
        if self.hangup_reason.is_none() {
            self.hangup_reason = Some(reason);
        }
    }

    pub fn merge_option(&self, mut option: CallOption) -> CallOption {
        if let Some(existing) = &self.option {
            if option.asr.is_none() {
                option.asr = existing.asr.clone();
            }
            if option.tts.is_none() {
                option.tts = existing.tts.clone();
            }
            if option.vad.is_none() {
                option.vad = existing.vad.clone();
            }
            if option.denoise.is_none() {
                option.denoise = existing.denoise;
            }
            if option.agc.is_none() {
                option.agc = existing.agc.clone();
            }
            if option.recorder.is_none() {
                option.recorder = existing.recorder.clone();
            }
            if option.eou.is_none() {
                option.eou = existing.eou.clone();
            }
            if option.extra.is_none() {
                option.extra = existing.extra.clone();
            }
            if option.ambiance.is_none() {
                option.ambiance = existing.ambiance.clone();
            }
            if option.ringback_detection.is_none() {
                option.ringback_detection = existing.ringback_detection.clone();
            }
        }
        option
    }

    pub fn termination(reason: Option<&TerminatedReason>) -> TerminationInfo {
        match reason {
            Some(TerminatedReason::UacCancel) => {
                TerminationInfo::new(487, CallRecordHangupReason::Canceled, "caller")
            }
            Some(TerminatedReason::UacBye) => {
                TerminationInfo::new(200, CallRecordHangupReason::ByCaller, "caller")
            }
            Some(TerminatedReason::UacBusy) => {
                TerminationInfo::new(486, CallRecordHangupReason::ByCaller, "caller")
            }
            Some(TerminatedReason::UasBye) => {
                TerminationInfo::new(200, CallRecordHangupReason::ByCallee, "callee")
            }
            Some(TerminatedReason::UasBusy) => {
                TerminationInfo::new(486, CallRecordHangupReason::ByCallee, "callee")
            }
            Some(TerminatedReason::UasDecline) => {
                TerminationInfo::new(603, CallRecordHangupReason::ByCallee, "callee")
            }
            Some(TerminatedReason::UacOther(code)) => {
                TerminationInfo::new(code.code(), CallRecordHangupReason::ByCaller, "system")
            }
            Some(TerminatedReason::UasOther(code)) => {
                TerminationInfo::new(code.code(), CallRecordHangupReason::ByCallee, "system")
            }
            _ => TerminationInfo::new(500, CallRecordHangupReason::BySystem, "system"),
        }
    }
}

pub struct TerminationInfo {
    pub status_code: u16,
    pub hangup_reason: CallRecordHangupReason,
    pub initiator: &'static str,
}

impl TerminationInfo {
    fn new(
        status_code: u16,
        hangup_reason: CallRecordHangupReason,
        initiator: &'static str,
    ) -> Self {
        Self {
            status_code,
            hangup_reason,
            initiator,
        }
    }
}

#[derive(Clone)]
pub struct LegShared {
    pub ssrc: u32,
    pub is_refer: bool,
    pub progress: Arc<ArcSwap<CallProgress>>,
    pub extras: Extras,
}

impl LegShared {
    pub fn new(ssrc: u32, is_refer: bool, progress: CallProgress) -> Self {
        Self {
            ssrc,
            is_refer,
            progress: Arc::new(ArcSwap::from_pointee(progress)),
            extras: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    pub fn update_progress(&self, f: impl Fn(&mut CallProgress)) {
        self.progress.rcu(|p| {
            let mut p = CallProgress::clone(p);
            f(&mut p);
            p
        });
    }

    pub fn set_extra(&self, key: &str, value: Value) {
        self.extras.rcu(|e| {
            let mut e = HashMap::clone(e);
            e.insert(key.to_string(), value.clone());
            e
        });
    }

    pub fn build_hangup_event(&self, track_id: TrackId, initiator: Option<String>) -> SessionEvent {
        let progress = self.progress.load_full();
        let extras = self.extras.load_full();
        build_hangup_event(&progress, &extras, self.is_refer, track_id, initiator)
    }

    pub fn build_callrecord(&self, app_state: &AppState, call_type: ActiveCallType) -> CallRecord {
        let session_id = self.progress.load_full().session_id.clone();
        self.build_callrecord_with_id(app_state, call_type, session_id)
    }

    pub fn build_callrecord_with_id(
        &self,
        app_state: &AppState,
        call_type: ActiveCallType,
        session_id: String,
    ) -> CallRecord {
        let progress = self.progress.load_full();
        let extras = self.extras.load_full();
        build_callrecord(&progress, &extras, None, app_state, session_id, call_type)
    }
}

pub fn build_hangup_event(
    progress: &CallProgress,
    extras: &HashMap<String, Value>,
    is_refer: bool,
    track_id: TrackId,
    initiator: Option<String>,
) -> SessionEvent {
    let from = progress.option.as_ref().and_then(|o| o.caller.as_ref());
    let to = progress.option.as_ref().and_then(|o| o.callee.as_ref());

    SessionEvent::Hangup {
        track_id,
        timestamp: crate::media::get_timestamp(),
        reason: progress.hangup_reason.as_ref().map(|r| format!("{:?}", r)),
        initiator,
        start_time: progress.start_time.unwrap_or_default().to_rfc3339(),
        answer_time: progress.answer_time.map(|t| t.to_rfc3339()),
        ringing_time: progress.ring_time.map(|t| t.to_rfc3339()),
        hangup_time: Utc::now().to_rfc3339(),
        extra: Some(extras.clone()),
        from: from.map(|f| f.into()),
        to: to.map(|f| f.into()),
        refer: Some(is_refer),
    }
}

pub fn build_callrecord(
    progress: &CallProgress,
    extras: &HashMap<String, Value>,
    refer_leg: Option<&LegShared>,
    app_state: &AppState,
    session_id: String,
    call_type: ActiveCallType,
) -> CallRecord {
    let option = progress.option.clone().unwrap_or_default();
    let recorder = if option.recorder.is_some() {
        let recorder_file = app_state.get_recorder_file(&session_id);
        if std::path::Path::new(&recorder_file).exists() {
            let file_size = std::fs::metadata(&recorder_file)
                .map(|m| m.len())
                .unwrap_or(0);
            vec![crate::callrecord::CallRecordMedia {
                track_id: session_id.clone(),
                path: recorder_file,
                size: file_size,
                extra: None,
            }]
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let dump_event_file = app_state.get_dump_events_file(&session_id);
    let dump_event_file = if std::path::Path::new(&dump_event_file).exists() {
        Some(dump_event_file)
    } else {
        None
    };

    let refer_callrecord =
        refer_leg.map(|leg| Box::new(leg.build_callrecord(app_state, ActiveCallType::B2bua)));

    let caller = option.caller.clone().unwrap_or_default();
    let callee = option.callee.clone().unwrap_or_default();

    CallRecord {
        option: Some(option),
        call_id: session_id,
        call_type,
        start_time: progress.start_time.unwrap_or_default(),
        ring_time: progress.ring_time,
        answer_time: progress.answer_time,
        end_time: Utc::now(),
        caller,
        callee,
        hangup_reason: progress.hangup_reason.clone(),
        hangup_messages: Vec::new(),
        status_code: progress.last_status_code,
        extras: Some(extras.clone()),
        dump_event_file,
        recorder,
        refer_callrecord,
    }
}

pub fn resolve_final_answer(
    raw: Option<Vec<u8>>,
    early: Option<&String>,
) -> Result<(String, bool), &'static str> {
    match raw {
        Some(bytes) => {
            let s = String::from_utf8_lossy(&bytes).to_string();
            if s.trim().is_empty() {
                match early {
                    Some(e) if !e.is_empty() => Ok((e.clone(), true)),
                    _ => Ok((s, false)),
                }
            } else {
                Ok((s, false))
            }
        }
        None => match early {
            Some(e) if !e.is_empty() => Ok((e.clone(), true)),
            _ => Err("no answer received"),
        },
    }
}

pub enum ActorMsg {
    ReferDone {
        track_id: TrackId,
        forward_dtmf: bool,
        result: Result<String, rsipstack::Error>,
    },
}

pub struct CallRuntime {
    pub input_timeout_expire: (u64, u32),
    pub actor_tx: mpsc::Sender<ActorMsg>,
    pub me: Option<crate::call::active_call::ActiveCallRef>,
}

impl CallRuntime {
    pub fn new(actor_tx: mpsc::Sender<ActorMsg>) -> Self {
        Self {
            input_timeout_expire: (0, 0),
            actor_tx,
            me: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EARLY_SDP: &str = "v=0\r\no=- 1000 1 IN IP4 192.168.1.100\r\n";

    #[test]
    fn early_then_confirmed_progress() {
        let mut p = CallProgress::default();
        p.on_early(183);
        assert!(p.ring_time.is_some());
        assert_eq!(p.last_status_code, 183);

        p.on_confirmed("dialog-1".to_string());
        assert_eq!(p.session_id, "dialog-1");
        assert_eq!(p.last_status_code, 200);
        assert!(p.answer_time.is_some());

        // A second confirmed must not move answer_time.
        let first = p.answer_time;
        p.on_confirmed("dialog-2".to_string());
        assert_eq!(p.answer_time, first);
    }

    #[test]
    fn try_set_answer_keeps_first() {
        let mut p = CallProgress::default();
        p.try_set_answer(EARLY_SDP);
        p.try_set_answer("late");
        assert_eq!(p.answer.as_deref(), Some(EARLY_SDP));
    }

    #[test]
    fn set_hangup_reason_keeps_first() {
        let mut p = CallProgress::default();
        p.set_hangup_reason(CallRecordHangupReason::ByCaller);
        p.set_hangup_reason(CallRecordHangupReason::ByCallee);
        assert_eq!(p.hangup_reason, Some(CallRecordHangupReason::ByCaller));
    }

    #[test]
    fn resolve_final_answer_uses_early_sdp_on_empty_body() {
        let early = EARLY_SDP.to_string();
        let (sdp, applied) = resolve_final_answer(Some(vec![]), Some(&early)).unwrap();
        assert_eq!(sdp, EARLY_SDP);
        assert!(applied);
    }

    #[test]
    fn resolve_final_answer_uses_200ok_body_when_present() {
        let early = EARLY_SDP.to_string();
        let (sdp, applied) = resolve_final_answer(Some(b"final".to_vec()), Some(&early)).unwrap();
        assert_eq!(sdp, "final");
        assert!(!applied);
    }

    #[test]
    fn resolve_final_answer_no_answer_at_all() {
        assert!(resolve_final_answer(None, None).is_err());
        assert!(resolve_final_answer(None, Some(&String::new())).is_err());
    }

    #[test]
    fn termination_mapping() {
        let info = CallProgress::termination(Some(&TerminatedReason::UacCancel));
        assert_eq!((info.status_code, info.initiator), (487, "caller"));
        assert_eq!(info.hangup_reason, CallRecordHangupReason::Canceled);

        let info = CallProgress::termination(Some(&TerminatedReason::UasDecline));
        assert_eq!((info.status_code, info.initiator), (603, "callee"));
        assert_eq!(info.hangup_reason, CallRecordHangupReason::ByCallee);

        let info = CallProgress::termination(None);
        assert_eq!((info.status_code, info.initiator), (500, "system"));
        assert_eq!(info.hangup_reason, CallRecordHangupReason::BySystem);
    }

    #[test]
    fn leg_shared_extras_rcu() {
        let leg = LegShared::new(42, false, CallProgress::default());
        leg.set_extra("k", Value::String("v".into()));
        assert_eq!(
            leg.extras.load_full().get("k"),
            Some(&Value::String("v".into()))
        );
    }
}
