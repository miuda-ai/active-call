use super::Command;
use crate::{
    CallOption, ReferOption,
    call::state::{ActorMsg, CallProgress, CallRuntime, Extras, LegShared, build_callrecord},
    event::{EventReceiver, EventSender, SessionEvent},
    media::{
        TrackId,
        engine::StreamEngine,
        recorder::RecorderOption,
        stream::{MediaStream, MediaStreamBuilder, SERVER_SIDE_TRACK_ID},
        track::{
            Track, TrackConfig, forwarding::ForwardingTrack, media_pass::MediaPassTrack,
            tts::SynthesisHandle, websocket::WebsocketBytesReceiver,
        },
    },
    synthesis::SynthesisCommand,
    transcription::TranscriptionOption,
};
use crate::{
    app::AppState,
    call::{CommandReceiver, CommandSender, sip::Invitation},
    callrecord::{CallRecord, CallRecordEvent, CallRecordEventType, CallRecordHangupReason},
};
use anyhow::Result;
use arc_swap::{ArcSwap, ArcSwapOption};
use chrono::{DateTime, Utc};
use rsipstack::dialog::invite_dialog::InviteDialog;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{fs::File, select, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Describes the state of the caller track when an incoming SIP call is waiting to be answered.
pub enum PendingCallerTrack {
    /// The track has been started in the media stream during ringing (early media).
    /// Processors must be built from the accept option and appended to it.
    StartedForEarlyMedia,
    /// The track has not been added to the media stream yet.
    /// setup_track_with_stream will start it and build processors from the accept option.
    NotStarted(Box<dyn Track>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppStateBuilder;
    use crate::config::Config;
    use crate::media::track::tts::SynthesisHandle;
    use crate::synthesis::SynthesisCommand;
    use tokio::sync::mpsc;

    async fn make_active_call_with_option(option: CallOption) -> Arc<ActiveCall> {
        let mut config = Config::default();
        config.udp_port = 0; // Use random port
        config.media_cache_path = "/tmp/mediacache".to_string();
        let app_state = AppStateBuilder::new()
            .with_config(config)
            .with_stream_engine(Arc::new(StreamEngine::default()))
            .build()
            .await
            .unwrap();
        let active_call = Arc::new(ActiveCall::new(CallSpec {
            call_type: ActiveCallType::Sip,
            cancel_token: CancellationToken::new(),
            session_id: "test-session".to_string(),
            invitation: app_state.invitation.clone(),
            app_state: app_state.clone(),
            track_config: TrackConfig::default(),
            audio_receiver: None,
            dump_events: false,
            server_side_track_id: None,
            extras: None,
        }));
        active_call.set_option(option);
        active_call
    }

    #[tokio::test]
    async fn test_tts_ssrc_reuse_for_autohangup() -> Result<()> {
        let mut option = crate::CallOption::default();
        option.tts = Some(crate::synthesis::SynthesisOption::default());
        let active_call = make_active_call_with_option(option).await;

        let (tx, mut rx) = mpsc::unbounded_channel::<SynthesisCommand>();
        let initial_ssrc = 12345;
        let handle = SynthesisHandle::new(tx, Some("play_1".to_string()), initial_ssrc);

        // 1. Set initial TTS handle
        active_call.tts_handle.store(Some(Arc::new(handle)));
        active_call.set_current_play(Some("play_1".to_string()));

        // 2. Call do_tts with auto_hangup=true and same play_id
        active_call
            .do_tts(Command::Tts {
                text: "hangup now".to_string(),
                speaker: None,
                play_id: Some("play_1".to_string()),
                auto_hangup: Some(true),
                streaming: Some(false),
                end_of_stream: Some(true),
                option: None,
                wait_input_timeout: None,
                base64: Some(false),
                cache_key: None,
            })
            .await?;

        // 3. Verify the hangup intent rides on the command sent to the existing track
        let cmd = rx.try_recv().expect("Should have received tts command");
        assert_eq!(cmd.text, "hangup now");
        assert_eq!(cmd.auto_hangup, Some(true));

        Ok(())
    }

    #[tokio::test]
    async fn test_tts_new_ssrc_for_different_play_id() -> Result<()> {
        let mut tts_opt = crate::synthesis::SynthesisOption::default();
        tts_opt.provider = Some(crate::synthesis::SynthesisType::Aliyun);
        let mut option = crate::CallOption::default();
        option.tts = Some(tts_opt);
        let active_call = make_active_call_with_option(option).await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let initial_ssrc = 111;
        let handle = SynthesisHandle::new(tx, Some("play_1".to_string()), initial_ssrc);

        active_call.tts_handle.store(Some(Arc::new(handle)));
        active_call.set_current_play(Some("play_1".to_string()));

        // Call do_tts with DIFFERENT play_id
        active_call
            .do_tts(Command::Tts {
                text: "new play".to_string(),
                speaker: None,
                play_id: Some("play_2".to_string()),
                auto_hangup: Some(true),
                streaming: Some(false),
                end_of_stream: Some(true),
                option: None,
                wait_input_timeout: None,
                base64: Some(false),
                cache_key: None,
            })
            .await?;

        // Verify a NEW track was started (new handle with a different ssrc,
        // because a different play_id interrupts and starts fresh)
        {
            let handle = active_call.tts_handle.load_full();
            assert!(handle.is_some(), "new tts handle should be stored");
            let handle = handle.unwrap();
            assert_ne!(
                handle.ssrc, initial_ssrc,
                "Should use a new SSRC for different play_id"
            );
        }

        Ok(())
    }

    // refer=Some(true): only the refer call is cancelled, media stream stays alive.
    #[tokio::test]
    async fn test_hangup_refer_true_cancels_refer_only() -> Result<()> {
        let active_call = make_active_call_with_option(crate::CallOption::default()).await;

        let refer_token = active_call.cancel_token.child_token();
        let refer_leg = LegShared::new(1, true, CallProgress::default());
        active_call.set_refer_call_token(refer_token.clone());
        active_call.set_refer_leg(Some(refer_leg.clone()));

        active_call.do_hangup(None, None, None, Some(true)).await?;

        assert!(
            refer_token.is_cancelled(),
            "refer token should be cancelled"
        );
        assert!(
            !active_call.media_stream.cancel_token.is_cancelled(),
            "media stream should NOT stop"
        );
        assert!(
            refer_leg.progress.load_full().hangup_reason.is_some(),
            "hangup_reason should be set on refer leg"
        );
        Ok(())
    }

    // refer=None: media stream stops and the refer token is also cancelled.
    #[tokio::test]
    async fn test_hangup_none_cancels_refer_too() -> Result<()> {
        let active_call = make_active_call_with_option(crate::CallOption::default()).await;

        let refer_token = active_call.cancel_token.child_token();
        active_call.set_refer_call_token(refer_token.clone());

        active_call.do_hangup(None, None, None, None).await?;

        assert!(
            refer_token.is_cancelled(),
            "refer token should be cancelled"
        );
        assert!(
            active_call.media_stream.cancel_token.is_cancelled(),
            "media stream should stop"
        );
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Regression: ringing-before-accept leaves caller track without processors
    // ---------------------------------------------------------------------------
    //
    // When Ringing is issued before Accept on an incoming SIP call,
    // prepare_incoming_sip_track starts the caller track in the media stream
    // (needed for early-media ringtone) with the empty ringing option — no
    // VAD/ASR/AGC processors.  It stores PendingCallerTrack::StartedForEarlyMedia
    // in ready_to_answer.  At accept time, finish_caller_stack matches that variant
    // and calls create_processors + append_processor with the real accept option.
    //
    // This test verifies that setup_track_with_stream (same processor-creation path)
    // fires the ASR builder when given the accept option, proving the fix is sound.

    struct MockCallerTrack {
        id: TrackId,
        config: crate::media::track::TrackConfig,
        processor_chain: crate::media::processor::ProcessorChain,
    }

    impl MockCallerTrack {
        fn new(id: TrackId) -> Self {
            Self {
                id,
                config: crate::media::track::TrackConfig::default(),
                processor_chain: crate::media::processor::ProcessorChain::new(16000),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::media::track::Track for MockCallerTrack {
        fn ssrc(&self) -> u32 {
            0
        }
        fn id(&self) -> &TrackId {
            &self.id
        }
        fn config(&self) -> &crate::media::track::TrackConfig {
            &self.config
        }
        fn processor_chain(&mut self) -> &mut crate::media::processor::ProcessorChain {
            &mut self.processor_chain
        }
        async fn handshake(
            &mut self,
            _o: String,
            _t: Option<tokio::time::Duration>,
        ) -> Result<String> {
            Ok(String::new())
        }
        async fn update_remote_description(&mut self, _a: &String) -> Result<()> {
            Ok(())
        }
        async fn start(
            &mut self,
            _e: crate::event::EventSender,
            _p: crate::media::track::TrackPacketSender,
        ) -> Result<()> {
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn send_packet(&mut self, _f: &crate::media::AudioFrame) -> Result<()> {
            Ok(())
        }
    }

    struct MockAsrClient;

    #[async_trait::async_trait]
    impl crate::transcription::TranscriptionClient for MockAsrClient {
        fn send_audio(
            &self,
            _s: &[crate::media::Sample],
            _src: Option<&crate::media::SourcePacket>,
        ) -> Result<()> {
            Ok(())
        }
    }

    async fn make_active_call_with_engine_and_option(
        engine: Arc<StreamEngine>,
        cache_dir: &str,
        option: crate::CallOption,
    ) -> Arc<ActiveCall> {
        let mut config = Config::default();
        config.udp_port = 0;
        config.media_cache_path = cache_dir.to_string();
        let app_state = AppStateBuilder::new()
            .with_config(config)
            .with_stream_engine(engine)
            .build()
            .await
            .unwrap();
        let session_id = format!("test-{}-{}", cache_dir, uuid::Uuid::new_v4());
        let active_call = Arc::new(ActiveCall::new(CallSpec {
            call_type: ActiveCallType::Sip,
            cancel_token: CancellationToken::new(),
            session_id: session_id.clone(),
            invitation: app_state.invitation.clone(),
            app_state: app_state.clone(),
            track_config: TrackConfig::default(),
            audio_receiver: None,
            dump_events: false,
            server_side_track_id: None,
            extras: None,
        }));
        active_call.set_option(option);
        active_call
    }

    #[tokio::test]
    async fn test_setup_track_with_stream_builds_processors_from_accept_option() -> Result<()> {
        let (asr_created_tx, mut asr_created_rx) = mpsc::channel::<()>(1);

        let mock_provider =
            crate::transcription::TranscriptionType::Other("mock-ringing-asr".to_string());

        let mut engine = StreamEngine::new();
        engine.register_asr(
            mock_provider.clone(),
            Box::new(move |_tid, _tok, _opt, _es| {
                let tx = asr_created_tx.clone();
                Box::pin(async move {
                    let _ = tx.send(()).await;
                    Ok(Box::new(MockAsrClient)
                        as Box<dyn crate::transcription::TranscriptionClient>)
                })
            }),
        );
        let engine = Arc::new(engine);

        let accept_option = crate::CallOption {
            asr: Some(crate::transcription::TranscriptionOption {
                provider: Some(mock_provider),
                ..Default::default()
            }),
            ..Default::default()
        };
        let active_call = make_active_call_with_engine_and_option(
            engine,
            "mediacache_ringing_accept_test",
            accept_option,
        )
        .await;
        let cancel_token = active_call.cancel_token.clone();

        // Simulate the fixed prepare_incoming_sip_track: track is held in
        // ready_to_answer, NOT yet added to the media stream.
        let mock_track = Box::new(MockCallerTrack::new(active_call.session_id.clone()));

        // Simulate finish_caller_stack at accept time: setup_track_with_stream
        // is called with the full accept option (the code path under test).
        let accept_option = active_call.progress.load_full().option.clone().unwrap();
        active_call
            .setup_track_with_stream(&accept_option, mock_track)
            .await?;

        // The mock ASR builder must have fired, proving processors were built
        // from the accept option and attached to the caller track.
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(3), asr_created_rx.recv()).await;
        assert!(
            received.is_ok() && received.unwrap().is_some(),
            "ASR processor was NOT created — setup_track_with_stream did not build \
             processors from the accept option (regression: ringing-before-accept)"
        );

        cancel_token.cancel();
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Regression: double-ASR when Accept arrives without a prior Ringing
    // ---------------------------------------------------------------------------
    //
    // prepare_incoming_sip_track starts the caller track during ringing. It must
    // NOT build VAD/ASR/AGC processors at that point: the stored option already
    // carries `asr` in the accept-first path, so finish_caller_stack would build a
    // second set at accept time, producing two ASR clients (two WebSocket
    // connections) on the same track.
    //
    // This test verifies that update_track_wrapper (the prepare-time path) never
    // fires the ASR builder even when the option carries an asr config.

    #[tokio::test]
    async fn test_update_track_wrapper_does_not_build_asr_processor() -> Result<()> {
        let (asr_created_tx, mut asr_created_rx) = mpsc::channel::<()>(1);

        let mock_provider =
            crate::transcription::TranscriptionType::Other("mock-ringing-asr".to_string());

        let mut engine = StreamEngine::new();
        engine.register_asr(
            mock_provider.clone(),
            Box::new(move |_tid, _tok, _opt, _es| {
                let tx = asr_created_tx.clone();
                Box::pin(async move {
                    let _ = tx.send(()).await;
                    Ok(Box::new(MockAsrClient)
                        as Box<dyn crate::transcription::TranscriptionClient>)
                })
            }),
        );
        let engine = Arc::new(engine);

        // Simulate the accept-first path: setup_caller_track has already stored the
        // full accept option (with asr) before the track is prepared.
        let accept_option = crate::CallOption {
            asr: Some(crate::transcription::TranscriptionOption {
                provider: Some(mock_provider),
                ..Default::default()
            }),
            ..Default::default()
        };
        let active_call = make_active_call_with_engine_and_option(
            engine,
            "mediacache_update_track_wrapper_test",
            accept_option,
        )
        .await;
        let cancel_token = active_call.cancel_token.clone();

        let mock_track = Box::new(MockCallerTrack::new(active_call.session_id.clone()));
        active_call.update_track_wrapper(mock_track, None).await;

        // update_track_wrapper must NOT fire the ASR builder; the processors are
        // deferred to finish_caller_stack(StartedForEarlyMedia) at accept time.
        let received =
            tokio::time::timeout(std::time::Duration::from_millis(500), asr_created_rx.recv())
                .await;
        assert!(
            received.is_err(),
            "ASR builder fired during track preparation — double-ASR regression"
        );

        cancel_token.cancel();
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallParams {
    pub id: Option<String>,
    #[serde(rename = "dump")]
    pub dump_events: Option<bool>,
    #[serde(rename = "ping")]
    pub ping_interval: Option<u32>,
    pub server_side_track: Option<String>,
    /// Set when this connection is a one-hop find from another node.
    /// Only an empty `forward` may be forwarded. A present value is answered
    /// locally only and must never hop again. `forward=true` must 404 if the
    /// session is absent (do not create a new call).
    #[serde(default)]
    pub forward: Option<bool>,
    /// Ignored. Kept so older nodes that still send `visited=` can deserialize.
    #[serde(default)]
    pub visited: Option<String>,
}

impl CallParams {
    /// Build the query string used when forwarding this request to a peer.
    /// Sets `forward=true` so the peer will not hop (`forward` is no longer empty).
    pub fn to_forward_query(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(id) = &self.id {
            parts.push(format!("id={}", urlencoding::encode(id)));
        }
        if let Some(dump) = self.dump_events {
            parts.push(format!("dump={}", dump));
        }
        if let Some(ping) = self.ping_interval {
            parts.push(format!("ping={}", ping));
        }
        if let Some(track) = &self.server_side_track {
            parts.push(format!("server_side_track={}", urlencoding::encode(track)));
        }
        parts.push("forward=true".to_string());
        parts.join("&")
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ActiveCallType {
    Webrtc,
    B2bua,
    WebSocket,
    #[default]
    Sip,
}

pub type ActiveCallRef = Arc<ActiveCall>;

/// A call: immutable identity + lock-free shared state (see `call::state`)
/// plus the infrastructure handles. Actor-owned mutable state lives in
/// [`CallRuntime`] inside `serve`.
pub struct ActiveCall {
    pub cancel_token: CancellationToken,
    pub call_type: ActiveCallType,
    pub session_id: String,
    pub start_time: DateTime<Utc>,
    pub media_stream: Arc<MediaStream>,
    pub track_config: TrackConfig,
    pub event_sender: EventSender,
    pub app_state: AppState,
    pub invitation: Invitation,
    pub cmd_sender: CommandSender,
    pub dump_events: bool,
    pub server_side_track_id: TrackId,

    /// Immutable main-leg SSRC.
    pub ssrc: u32,
    /// Bridge pause flag shared with the peer call during bridging.
    pub bridge_paused: Arc<AtomicBool>,
    /// Main-leg lifecycle progress (dialog id, ring/answer times, answer SDP, option...).
    pub progress: Arc<ArcSwap<CallProgress>>,
    /// Main-leg variables (playbook set_var, SIP headers, hangup headers).
    pub extras: Extras,
    /// Active music-on-hold path (shared with the spawned refer task).
    pub moh: ArcSwapOption<String>,
    /// play id of the current server-side playback (shared for TrackEnd matching).
    pub current_play_id: ArcSwapOption<String>,
    /// Live TTS handle (shared so spawned tasks and post-serve cleanup can drop it).
    pub tts_handle: ArcSwapOption<SynthesisHandle>,
    /// Shared state of the refer leg, when one is active.
    pub refer_leg: ArcSwapOption<LegShared>,
    /// Answer prepared during ringing (SDP + running track + dialog); taken
    /// by accept/reject. Shared so post-serve `cleanup` can still reject.
    pub ready_to_answer: ArcSwapOption<ReadyAnswer>,
    /// Answer prepared for the underlying SIP dialog of non-SIP call types
    /// (WebSocket/Webrtc); taken by accept, rejected on reject/cleanup.
    /// Same slot family as `ready_to_answer`: lock-free set/take on the
    /// actor path, with ownership recoverable from `cleanup`.
    pub pending_sip_answer: ArcSwapOption<PendingSipAnswer>,
    /// Cancel this token to hang up only the refer call, leaving the main call alive.
    pub refer_call_token: ArcSwapOption<CancellationToken>,
    /// Pending wait-input timeout set by the last Tts/Play command.
    pub wait_input_timeout: ArcSwapOption<u32>,
    /// ASR config to resume on the parent leg once the refer leg ends.
    pub pending_asr_resume: ArcSwapOption<(u32, TranscriptionOption)>,
    /// WebSocket audio receiver injected at construction, taken once by setup.
    pub audio_receiver: std::sync::Mutex<Option<WebsocketBytesReceiver>>,
}

impl ActiveCall {
    /// Lock-free shared state of the main leg.
    pub fn leg(&self) -> LegShared {
        LegShared {
            ssrc: self.ssrc,
            is_refer: false,
            progress: self.progress.clone(),
            extras: self.extras.clone(),
        }
    }

    /// Store/replace the main-leg option in the progress snapshot.
    pub fn set_option(&self, option: CallOption) {
        self.progress.rcu(|p| {
            let mut p = CallProgress::clone(p);
            p.option = Some(option.clone());
            p
        });
    }

    pub fn moh_path(&self) -> Option<String> {
        self.moh.load_full().map(|s| s.to_string())
    }

    pub fn set_moh(&self, v: Option<String>) {
        self.moh.store(v.map(Arc::new));
    }

    pub fn current_play(&self) -> Option<String> {
        self.current_play_id.load_full().map(|s| s.to_string())
    }

    pub fn set_current_play(&self, v: Option<String>) {
        self.current_play_id.store(v.map(Arc::new));
    }

    pub fn refer_leg_value(&self) -> Option<LegShared> {
        self.refer_leg.load_full().map(|l| l.as_ref().clone())
    }

    pub fn set_refer_leg(&self, v: Option<LegShared>) {
        self.refer_leg.store(v.map(Arc::new));
    }

    /// Prepared answer during ringing; taken once by accept/reject.
    pub fn set_ready_to_answer(&self, ready: ReadyAnswer) {
        self.ready_to_answer.store(Some(Arc::new(ready)));
    }

    pub fn take_ready_to_answer(&self) -> Option<Arc<ReadyAnswer>> {
        self.ready_to_answer.swap(None)
    }

    pub fn has_ready_to_answer(&self) -> bool {
        self.ready_to_answer.load().is_some()
    }

    /// Prepared 200 OK for the underlying SIP dialog of non-SIP call types.
    pub fn set_pending_sip_answer(&self, pending: PendingSipAnswer) {
        self.pending_sip_answer.store(Some(Arc::new(pending)));
    }

    pub fn take_pending_sip_answer(&self) -> Option<Arc<PendingSipAnswer>> {
        self.pending_sip_answer.swap(None)
    }

    /// Cancel token that hangs up only the refer leg, leaving the main call
    /// alive; set by `do_refer`, taken by hangup.
    pub fn take_refer_call_token(&self) -> Option<CancellationToken> {
        self.refer_call_token.swap(None).map(|t| (*t).clone())
    }

    pub fn set_refer_call_token(&self, token: CancellationToken) {
        self.refer_call_token.store(Some(Arc::new(token)));
    }

    /// Pending wait-input timeout set by the last Tts/Play command, consumed
    /// when the track ends.
    pub fn take_wait_input_timeout(&self) -> Option<u32> {
        self.wait_input_timeout.swap(None).map(|t| *t)
    }

    pub fn set_wait_input_timeout(&self, v: Option<u32>) {
        self.wait_input_timeout.store(v.map(Arc::new));
    }

    /// ASR config to resume on the parent leg once the refer leg ends.
    pub fn set_pending_asr_resume(&self, v: (u32, TranscriptionOption)) {
        self.pending_asr_resume.store(Some(Arc::new(v)));
    }

    pub fn take_pending_asr_resume(&self) -> Option<(u32, TranscriptionOption)> {
        self.pending_asr_resume.swap(None).map(|a| (*a).clone())
    }

    /// Insert/overwrite one main-leg extras variable.
    pub fn set_extra(&self, key: &str, value: serde_json::Value) {
        self.leg().set_extra(key, value);
    }

    /// Whether a pending (not yet answered) incoming dialog exists for this call.
    fn has_pending_invite(&self) -> bool {
        self.invitation
            .find_dialog_id_by_session_id(&self.session_id)
            .is_some()
    }

    /// One-shot hangup for error-cleanup paths outside the actor loop.
    async fn hangup_now(&self, reason: Option<CallRecordHangupReason>) {
        self.do_hangup(reason, None, None, None).await.ok();
    }

    /// One-shot reject for teardown paths outside the actor loop.
    async fn reject_now(&self, code: Option<rsipstack::rsip::StatusCode>, reason: Option<String>) {
        self.do_reject(code, reason).await.ok();
    }
}

/// Answer prepared during ringing: the SDP to answer with, the already
/// running caller track (started for early media), and the dialog to accept.
pub struct ReadyAnswer {
    pub answer: String,
    pub track: PendingCallerTrack,
    pub dialog: InviteDialog,
}

/// Answer prepared for the underlying inbound SIP dialog of a non-SIP call
/// type (WebSocket/Webrtc): the 200 OK SDP, the dialog to accept, and the RTP
/// track that bridges the SIP leg into the media stream.
pub struct PendingSipAnswer {
    pub answer: String,
    pub dialog: InviteDialog,
    pub track: Box<dyn Track>,
}

pub struct ActiveCallGuard {
    pub call: ActiveCallRef,
    pub active_calls: usize,
}

impl ActiveCallGuard {
    pub fn new(call: ActiveCallRef) -> Self {
        let active_calls = {
            call.app_state
                .total_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut calls = call.app_state.active_calls.lock().unwrap();
            calls.insert(call.session_id.clone(), call.clone());
            calls.len()
        };
        Self { call, active_calls }
    }
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        self.call
            .app_state
            .active_calls
            .lock()
            .unwrap()
            .remove(&self.call.session_id);
    }
}

pub struct ActiveCallReceiver {
    pub cmd_receiver: CommandReceiver,
    pub dump_cmd_receiver: CommandReceiver,
    pub dump_event_receiver: EventReceiver,
}

/// Construction parameters for [`ActiveCall::new`].
pub struct CallSpec {
    pub call_type: ActiveCallType,
    pub cancel_token: CancellationToken,
    pub session_id: String,
    pub invitation: Invitation,
    pub app_state: AppState,
    pub track_config: TrackConfig,
    /// WebSocket audio receiver (WebSocket calls only), taken once by setup.
    pub audio_receiver: Option<WebsocketBytesReceiver>,
    pub dump_events: bool,
    /// Overrides the default server-side track id.
    pub server_side_track_id: Option<TrackId>,
    /// Initial session variables; built-ins (session id, call type, start
    /// time) are injected for missing keys.
    pub extras: Option<HashMap<String, serde_json::Value>>,
}

impl ActiveCall {
    pub fn new(spec: CallSpec) -> Self {
        let CallSpec {
            call_type,
            cancel_token,
            session_id,
            invitation,
            app_state,
            track_config,
            audio_receiver,
            dump_events,
            server_side_track_id,
            extras,
        } = spec;
        let event_sender = crate::event::create_event_sender();
        let cmd_sender = tokio::sync::broadcast::Sender::<Command>::new(32);
        let server_side_track_id = server_side_track_id.unwrap_or(SERVER_SIDE_TRACK_ID.to_string());
        let media_stream_builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.child_token());
        let media_stream = Arc::new(media_stream_builder.build());
        let start_time = Utc::now();
        // Inject built-in session variables into extras
        let call_type_str = match &call_type {
            ActiveCallType::Sip => "sip",
            ActiveCallType::WebSocket => "websocket",
            ActiveCallType::Webrtc => "webrtc",
            ActiveCallType::B2bua => "b2bua",
        };
        let mut extras = extras.unwrap_or_default();
        extras
            .entry(crate::playbook::BUILTIN_SESSION_ID.to_string())
            .or_insert_with(|| serde_json::Value::String(session_id.clone()));
        extras
            .entry(crate::playbook::BUILTIN_CALL_TYPE.to_string())
            .or_insert_with(|| serde_json::Value::String(call_type_str.to_string()));
        extras
            .entry(crate::playbook::BUILTIN_START_TIME.to_string())
            .or_insert_with(|| serde_json::Value::String(start_time.to_rfc3339()));

        let progress = CallProgress {
            session_id: session_id.clone(),
            start_time: Some(start_time),
            ..Default::default()
        };

        Self {
            cancel_token,
            call_type,
            session_id,
            start_time,
            media_stream,
            track_config,
            event_sender,
            app_state,
            invitation,
            cmd_sender,
            dump_events,
            server_side_track_id,
            ssrc: rand::random::<u32>(),
            bridge_paused: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(ArcSwap::from_pointee(progress)),
            extras: Arc::new(ArcSwap::from_pointee(extras)),
            moh: ArcSwapOption::new(None),
            current_play_id: ArcSwapOption::new(None),
            tts_handle: ArcSwapOption::new(None),
            refer_leg: ArcSwapOption::new(None),
            ready_to_answer: ArcSwapOption::new(None),
            pending_sip_answer: ArcSwapOption::new(None),
            refer_call_token: ArcSwapOption::new(None),
            wait_input_timeout: ArcSwapOption::new(None),
            pending_asr_resume: ArcSwapOption::new(None),
            audio_receiver: std::sync::Mutex::new(audio_receiver),
        }
    }

    pub async fn enqueue_command(&self, command: Command) -> Result<()> {
        self.cmd_sender
            .send(command)
            .map_err(|e| anyhow::anyhow!("Failed to send command: {}", e))?;
        Ok(())
    }

    /// Create a new ActiveCallReceiver for this ActiveCall
    /// `tokio::sync::broadcast` not cached messages, so need to early create receiver
    /// before calling `serve()`
    pub fn new_receiver(&self) -> ActiveCallReceiver {
        ActiveCallReceiver {
            cmd_receiver: self.cmd_sender.subscribe(),
            dump_cmd_receiver: self.cmd_sender.subscribe(),
            dump_event_receiver: self.event_sender.subscribe(),
        }
    }

    /// The call actor: a single select loop owning the [`CallRuntime`].
    ///
    /// Commands, session events, background-completion messages, the
    /// wait-input timeout tick, the media stream and cancellation all meet in
    /// one place, so runtime state is plain (lock-free) data owned by this
    /// task — mirroring the concurrency of the previous separate
    /// command/event loops without their shared locks.
    pub async fn serve(self: Arc<Self>, receiver: ActiveCallReceiver) -> Result<()> {
        let ActiveCallReceiver {
            mut cmd_receiver,
            dump_cmd_receiver,
            dump_event_receiver,
        } = receiver;

        let mut event_receiver = self.event_sender.subscribe();
        let (actor_tx, mut actor_rx) = mpsc::channel::<ActorMsg>(16);
        let mut runtime = CallRuntime::new(actor_tx);
        runtime.me = Some(self.clone());

        self.app_state
            .total_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let me = self.clone();
        let actor = async move {
            // RAII: whatever way the loop exits (break, panic, early drop),
            // the call's cancel token fires so every child task and the media
            // stream shut down.
            let _cancel_on_exit = CancelOnExit(&me.cancel_token);
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            // Keep the media-serve future alive across select iterations.
            let mut media_serve = Box::pin(me.media_stream.serve());
            loop {
                tokio::select! {
                    cmd = cmd_receiver.recv() => {
                        match cmd {
                            Ok(command) => {
                                // Box::pin keeps the deep do_* future tree off
                                // the select's stack frame (debug builds overflow
                                // otherwise).
                                if let Err(e) = Box::pin(me.dispatch(&mut runtime, command)).await {
                                    warn!(session_id = me.session_id, "{}", e);
                                    me.event_sender
                                        .send(SessionEvent::Error {
                                            track_id: me.session_id.clone(),
                                            timestamp: crate::media::get_timestamp(),
                                            sender: "command".to_string(),
                                            error: e.to_string(),
                                            code: None,
                                        })
                                        .ok();
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => {
                                info!(session_id = me.session_id, "command loop done");
                                break;
                            }
                        }
                    }
                    ev = event_receiver.recv() => {
                        match ev {
                            Ok(event) => Box::pin(me.handle_event(&mut runtime, event)).await,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => {
                                info!(session_id = me.session_id, "event loop done");
                                break;
                            }
                        }
                    }
                    Some(msg) = actor_rx.recv() => {
                        if let Err(e) = Box::pin(me.handle_actor_msg(msg)).await {
                            warn!(session_id = me.session_id, "{}", e);
                            me.event_sender
                                .send(SessionEvent::Error {
                                    track_id: me.session_id.clone(),
                                    timestamp: crate::media::get_timestamp(),
                                    sender: "command".to_string(),
                                    error: e.to_string(),
                                    code: None,
                                })
                                .ok();
                        }
                    }
                    _ = ticker.tick() => {
                        Box::pin(me.check_input_timeout(&mut runtime)).await;
                    }
                    _ = &mut media_serve => {
                        info!(session_id = me.session_id, "media stream loop done");
                        break;
                    }
                    _ = me.cancel_token.cancelled() => {
                        info!(session_id = me.session_id, "call cancelled - cleaning up resources");
                        break;
                    }
                }
            }
        };

        tokio::join!(
            self.dump_loop(self.dump_events, dump_cmd_receiver, dump_event_receiver),
            actor
        );
        Ok(())
    }

    /// Wait-input silence timeout tick (formerly its own loop + mutex).
    async fn check_input_timeout(&self, runtime: &mut CallRuntime) {
        let (start_time, expire) = runtime.input_timeout_expire;
        if expire > 0 && crate::media::get_timestamp() >= start_time + expire as u64 {
            info!(session_id = self.session_id, "wait input timeout reached");
            runtime.input_timeout_expire = (0, 0);
            self.event_sender
                .send(SessionEvent::Silence {
                    track_id: self.server_side_track_id.clone(),
                    timestamp: crate::media::get_timestamp(),
                    start_time,
                    duration: expire as u64,
                    samples: None,
                    refer: Some(false),
                })
                .ok();
        }
    }

    /// Handle a session event (formerly the concurrent event-hook loop).
    async fn handle_event(&self, runtime: &mut CallRuntime, event: SessionEvent) {
        match event {
            SessionEvent::Speaking { .. }
            | SessionEvent::Dtmf { .. }
            | SessionEvent::AsrDelta { .. }
            | SessionEvent::AsrFinal { .. }
            | SessionEvent::TrackStart { .. } => {
                runtime.input_timeout_expire = (0, 0);
            }
            SessionEvent::TrackEnd {
                track_id,
                play_id,
                ssrc,
                auto_hangup,
                ..
            } => {
                if track_id != self.server_side_track_id {
                    return;
                }

                if play_id != self.current_play() {
                    debug!(
                        session_id = self.session_id,
                        ?play_id,
                        current = ?self.current_play(),
                        "ignoring interrupted track end"
                    );
                    return;
                }
                self.set_current_play(None);
                let moh_path = self.moh_path();
                let wait_timeout_val = self.take_wait_input_timeout();

                if let Some(path) = moh_path {
                    info!(session_id = self.session_id, "looping moh: {}", path);
                    let ssrc = rand::random::<u32>();
                    let file_track = self.make_file_track(path.clone(), ssrc);
                    self.update_track_wrapper(Box::new(file_track), Some(path))
                        .await;
                    return;
                }

                if let Some(hangup_reason) = auto_hangup {
                    info!(
                        session_id = self.session_id,
                        ssrc, "auto hangup when track end track_id:{}", track_id
                    );
                    self.do_hangup(Some(hangup_reason), None, None, None)
                        .await
                        .ok();
                }

                if let Some(timeout) = wait_timeout_val {
                    runtime.input_timeout_expire = if timeout > 0 {
                        (crate::media::get_timestamp(), timeout)
                    } else {
                        (0, 0)
                    };
                }
            }
            SessionEvent::Interrupt { receiver } => {
                let track_id = receiver.unwrap_or_else(|| self.server_side_track_id.clone());
                if track_id == self.server_side_track_id {
                    debug!(
                        session_id = self.session_id,
                        "received interrupt event, stopping playback"
                    );
                    self.do_interrupt(true).await.ok();
                }
            }
            SessionEvent::Inactivity { track_id, .. } => {
                info!(
                    session_id = self.session_id,
                    track_id, "inactivity timeout reached, hanging up"
                );
                self.do_hangup(
                    Some(CallRecordHangupReason::InactivityTimeout),
                    None,
                    None,
                    None,
                )
                .await
                .ok();
            }
            SessionEvent::Hangup { refer, .. } => {
                // Check if we need to resume ASR after refer hangup
                if refer == Some(true) {
                    if let Some((refer_ssrc, asr_option)) = self.take_pending_asr_resume() {
                        // Verify it's the refer call that ended
                        let is_refer_hangup = self
                            .refer_leg
                            .load_full()
                            .map(|leg| leg.ssrc == refer_ssrc)
                            .unwrap_or(false);

                        if is_refer_hangup {
                            info!(
                                session_id = self.session_id,
                                "Refer call ended, resuming parent ASR"
                            );

                            // Resume ASR
                            match self
                                .app_state
                                .stream_engine
                                .create_asr_processor(
                                    self.server_side_track_id.clone(),
                                    self.cancel_token.child_token(),
                                    asr_option,
                                    self.event_sender.clone(),
                                )
                                .await
                            {
                                Ok(asr_processor) => {
                                    if let Err(e) = self
                                        .media_stream
                                        .append_processor(&self.server_side_track_id, asr_processor)
                                        .await
                                    {
                                        warn!(
                                            session_id = self.session_id,
                                            "Failed to resume ASR after refer: {}", e
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        session_id = self.session_id,
                                        "Failed to create ASR processor for resume: {}", e
                                    );
                                }
                            }
                        }
                    }
                }
            }
            SessionEvent::Error { track_id, .. } => {
                if track_id != self.server_side_track_id {
                    return;
                }

                let moh_info = {
                    let path = self.moh_path();
                    path.map(|path| {
                        let fallback = "./config/sounds/refer_moh.wav".to_string();
                        if path != fallback && std::path::Path::new(&fallback).exists() {
                            info!(
                                session_id = self.session_id,
                                "moh error, switching to fallback: {}", fallback
                            );
                            self.set_moh(Some(fallback.clone()));
                            fallback
                        } else {
                            info!(
                                session_id = self.session_id,
                                "looping moh on error: {}", path
                            );
                            path
                        }
                    })
                };

                if let Some(next_path) = moh_info {
                    let ssrc = rand::random::<u32>();
                    let file_track = self.make_file_track(next_path.clone(), ssrc);
                    self.update_track_wrapper(Box::new(file_track), Some(next_path))
                        .await;
                }
            }
            SessionEvent::Hold { on_hold, .. } => {
                self.bridge_paused.store(on_hold, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Completion of background work spawned by a `do_*` command.
    async fn handle_actor_msg(&self, msg: ActorMsg) -> Result<()> {
        match msg {
            ActorMsg::ReferDone {
                track_id,
                forward_dtmf,
                result,
            } => match result {
                Ok(answer) => {
                    self.media_stream
                        .set_track_refer(&track_id, Some(true))
                        .await;
                    if !forward_dtmf {
                        self.media_stream
                            .set_track_dtmf_forward(&track_id, false)
                            .await;
                    }
                    self.event_sender
                        .send(SessionEvent::Answer {
                            timestamp: crate::media::get_timestamp(),
                            track_id,
                            sdp: answer,
                            refer: Some(true),
                        })
                        .ok();
                    Ok(())
                }
                Err(e) => {
                    warn!(
                        session_id = self.session_id,
                        "failed to create refer sip track: {}", e
                    );
                    self.emit_reject_from_rsip_error(track_id, true, &e);
                    Err(e.into())
                }
            },
        }
    }

    async fn dispatch(&self, runtime: &mut CallRuntime, command: Command) -> Result<()> {
        match command {
            Command::Invite { option } => self.do_invite(runtime, option).await,
            Command::Accept { option } => self.do_accept(option).await,
            Command::Reject { reason, code } => {
                self.do_reject(code.map(|c| (c as u16).into()), Some(reason))
                    .await
            }
            Command::Ringing { .. } => self.do_ringing(command).await,
            Command::Tts { .. } => self.do_tts(command).await,
            Command::Play { .. } => self.do_play(command).await,
            Command::Hangup {
                reason,
                initiator,
                headers,
                refer,
            } => {
                let reason = reason.map(|r| {
                    r.parse::<CallRecordHangupReason>()
                        .unwrap_or(CallRecordHangupReason::BySystem)
                });
                self.do_hangup(reason, initiator, headers, refer).await
            }
            Command::Refer {
                caller,
                callee,
                options,
            } => self.do_refer(runtime, caller, callee, options).await,
            Command::Message {
                body,
                content_type,
                headers,
                refer,
            } => self.do_message(body, content_type, headers, refer).await,
            Command::Bridge { target_session_id } => self.do_bridge(target_session_id).await,
            Command::Unbridge { target_session_id } => self.do_unbridge(target_session_id).await,
            Command::Mute { track_id } => self.do_mute(track_id).await,
            Command::Unmute { track_id } => self.do_unmute(track_id).await,
            Command::Pause {} => self.do_pause().await,
            Command::Resume {} => self.do_resume().await,
            Command::Interrupt {
                graceful: passage,
                fade_out_ms: _,
            } => self.do_interrupt(passage.unwrap_or_default()).await,
            Command::History { speaker, text } => self.do_history(speaker, text).await,
            Command::Custom { sender, data } => self.do_custom(sender, data),
            Command::AddIceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                self.media_stream
                    .add_ice_candidate(&candidate, sdp_mid.as_deref(), sdp_mline_index)
                    .await
            }
        }
    }

    fn build_record_option(&self, option: &CallOption) -> Option<RecorderOption> {
        if let Some(recorder_option) = &option.recorder {
            let mut recorder_file = recorder_option.recorder_file.clone();
            if recorder_file.contains("{id}") {
                recorder_file = recorder_file.replace("{id}", &self.session_id);
            }

            let recorder_file = if recorder_file.is_empty() {
                self.app_state.get_recorder_file(&self.session_id)
            } else {
                let p = Path::new(&recorder_file);
                p.is_absolute()
                    .then(|| recorder_file.clone())
                    .unwrap_or_else(|| self.app_state.get_recorder_file(&recorder_file))
            };
            info!(
                session_id = self.session_id,
                recorder_file, "created recording file"
            );

            let track_samplerate = self.track_config.samplerate;
            let recorder_samplerate = if track_samplerate > 0 {
                track_samplerate
            } else {
                recorder_option.samplerate
            };
            let recorder_ptime = if recorder_option.ptime == 0 {
                200
            } else {
                recorder_option.ptime
            };
            let requested_format = recorder_option
                .format
                .unwrap_or(self.app_state.config.recorder_format());
            let format = requested_format.effective();
            if requested_format != format {
                warn!(
                    session_id = self.session_id,
                    requested = requested_format.extension(),
                    "Recorder format fallback to wav due to unsupported feature"
                );
            }
            let mut recorder_config = RecorderOption {
                recorder_file,
                samplerate: recorder_samplerate,
                ptime: recorder_ptime,
                format: Some(format),
                native_samplerate: Some(
                    recorder_option.native_samplerate.unwrap_or(false)
                        || self.app_state.config.recorder_native_samplerate(),
                ),
            };
            recorder_config.ensure_path_extension(format);
            Some(recorder_config)
        } else {
            None
        }
    }

    async fn invite_or_accept(&self, mut option: CallOption, sender: String) -> Result<CallOption> {
        // Merge with existing configuration (e.g., from playbook)
        {
            let state = self.progress.load_full();
            option = state.merge_option(option);
        }

        option.check_default();
        if let Some(opt) = self.build_record_option(&option) {
            self.media_stream.update_recorder_option(opt).await;
        }
        self.ensure_call_ambiance(&option).await;

        if let Some(opt) = &option.media_pass {
            let track_id = self.server_side_track_id.clone();
            let cancel_token = self.cancel_token.child_token();
            let ssrc = rand::random::<u32>();
            let media_pass_track = MediaPassTrack::new(
                self.session_id.clone(),
                ssrc,
                track_id,
                cancel_token,
                opt.clone(),
            );
            self.update_track_wrapper(Box::new(media_pass_track), None)
                .await;
        }

        info!(
            session_id = self.session_id,
            call_type = ?self.call_type,
            sender,
            ?option,
            "caller with option"
        );

        match self.setup_caller_track(&option).await {
            Ok(_) => return Ok(option),
            Err(e) => {
                self.app_state
                    .total_failed_calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let error_event = crate::event::SessionEvent::Error {
                    track_id: self.session_id.clone(),
                    timestamp: crate::media::get_timestamp(),
                    sender,
                    error: e.to_string(),
                    code: None,
                };
                self.event_sender.send(error_event).ok();
                self.hangup_now(Some(CallRecordHangupReason::BySystem))
                    .await;
                return Err(e);
            }
        }
    }

    async fn do_invite(&self, runtime: &mut CallRuntime, option: CallOption) -> Result<()> {
        // Run the INVITE handshake in the background so the actor keeps
        // serving media (and everything else) while the call rings - same
        // reasoning as do_refer below. invite_or_accept blocks on the SIP
        // transaction for the entire ring duration; handling that inline on
        // the actor's own select loop would freeze media_stream.serve() (and
        // therefore all live audio bridging for this leg) for just as long.
        let me = runtime
            .me
            .clone()
            .ok_or_else(|| anyhow::anyhow!("invite is only supported inside serve()"))?;
        crate::spawn(async move {
            if let Err(e) = me.invite_or_accept(option, "invite".to_string()).await {
                warn!(session_id = me.session_id, "{}", e);
                me.event_sender
                    .send(SessionEvent::Error {
                        track_id: me.session_id.clone(),
                        timestamp: crate::media::get_timestamp(),
                        sender: "command".to_string(),
                        error: e.to_string(),
                        code: None,
                    })
                    .ok();
            }
        });
        Ok(())
    }

    async fn do_accept(&self, mut option: CallOption) -> Result<()> {
        let has_pending = self.has_pending_invite();
        let ready_to_answer_val = !self.has_ready_to_answer();

        if ready_to_answer_val {
            if !has_pending {
                // emit reject event
                warn!(session_id = self.session_id, "no pending call to accept");
                let rejet_event = crate::event::SessionEvent::Reject {
                    track_id: self.session_id.clone(),
                    timestamp: crate::media::get_timestamp(),
                    reason: "no pending call".to_string(),
                    refer: None,
                    code: Some(486),
                };
                self.event_sender.send(rejet_event).ok();
                self.hangup_now(Some(CallRecordHangupReason::BySystem))
                    .await;
                return Err(anyhow::anyhow!("no pending call to accept"));
            }
            option = self.invite_or_accept(option, "accept".to_string()).await?;
        } else {
            option.check_default();
            if let Some(opt) = self.build_record_option(&option) {
                self.media_stream.update_recorder_option(opt).await;
            }
            self.set_option(option.clone());
            self.ensure_call_ambiance(&option).await;
        }
        info!(session_id = self.session_id, ?option, "accepting call");
        let ready = self.take_ready_to_answer();
        if let Some(ready) = ready {
            // Exclusive since `take` swapped it out and the actor serializes
            // commands, so the unwrap cannot race another holder.
            let ReadyAnswer {
                answer,
                track: pending_track,
                dialog,
            } = match Arc::try_unwrap(ready) {
                Ok(ready) => ready,
                Err(_) => {
                    warn!(
                        session_id = self.session_id,
                        "ready_to_answer held elsewhere; skipping accept"
                    );
                    return Ok(());
                }
            };
            info!(session_id = self.session_id, "ready to answer with track");

            let headers = vec![rsipstack::rsip::Header::ContentType(
                "application/sdp".to_string().into(),
            )];

            match dialog.accept(Some(headers), Some(answer.as_bytes().to_vec())) {
                Ok(_) => {
                    self.leg().update_progress(|p| {
                        p.answer = Some(answer.clone());
                        p.answer_time.get_or_insert_with(Utc::now);
                    });
                    self.finish_caller_stack(&option, pending_track).await?;
                }
                Err(e) => {
                    warn!(session_id = self.session_id, "failed to accept call: {}", e);
                    return Err(anyhow::anyhow!("failed to accept call"));
                }
            }
        }

        // Non-SIP call types (WebSocket/Webrtc) created by an inbound SIP
        // INVITE never go through `ready_to_answer`; the underlying dialog is
        // answered here. Without this the carrier leg stays in `Trying` until
        // the far end times out and CANCELs (production: VOS3000 20s timeout).
        if let Some(pending) = self.take_pending_sip_answer() {
            let Ok(pending) = Arc::try_unwrap(pending) else {
                warn!(
                    session_id = self.session_id,
                    "pending sip answer held elsewhere; skipping sip accept"
                );
                return Ok(());
            };
            let headers = vec![rsipstack::rsip::Header::ContentType(
                "application/sdp".to_string().into(),
            )];
            match pending
                .dialog
                .accept(Some(headers), Some(pending.answer.as_bytes().to_vec()))
            {
                Ok(_) => {
                    info!(
                        session_id = self.session_id,
                        "answered underlying sip dialog"
                    );
                    self.leg().update_progress(|p| {
                        p.answer = Some(pending.answer.clone());
                        p.answer_time.get_or_insert_with(Utc::now);
                    });
                    // Register the SIP leg so customer audio is bridged with
                    // the caller track (and later the refer leg).
                    self.media_stream.update_track(pending.track, None).await;
                }
                Err(e) => {
                    warn!(
                        session_id = self.session_id,
                        "failed to accept underlying sip dialog: {}", e
                    );
                }
            }
        }
        Ok(())
    }

    async fn do_reject(
        &self,
        code: Option<rsipstack::rsip::StatusCode>,
        reason: Option<String>,
    ) -> Result<()> {
        if let Some(pending) = self.take_pending_sip_answer() {
            info!(
                session_id = self.session_id,
                ?reason,
                ?code,
                "rejecting underlying sip dialog"
            );
            if let Ok(pending) = Arc::try_unwrap(pending) {
                pending.dialog.reject(code.clone(), reason.clone()).ok();
                self.invitation.dialog_layer.remove_dialog(&pending.dialog.id());
            }
        }
        match self
            .invitation
            .find_dialog_id_by_session_id(&self.session_id)
        {
            Some(id) => {
                info!(
                    session_id = self.session_id,
                    ?reason,
                    ?code,
                    "rejecting call"
                );
                let result = self.invitation.hangup(id, code, reason).await;
                if result.is_ok() {
                    self.cancel_token.cancel();
                }
                result
            }
            None => {
                if let Some(ready) = self.take_ready_to_answer() {
                    info!(
                        session_id = self.session_id,
                        ?reason,
                        ?code,
                        "rejecting call from ready_to_answer"
                    );
                    let dialog = &ready.dialog;
                    let dialog_id = dialog.id();
                    dialog.reject(code, reason).ok();
                    self.invitation.dialog_layer.remove_dialog(&dialog_id);
                    self.cancel_token.cancel();
                }
                Ok(())
            }
        }
    }

    async fn do_ringing(&self, command: Command) -> Result<()> {
        let Command::Ringing {
            ringtone,
            recorder,
            early_media,
        } = command
        else {
            unreachable!("do_ringing called with non-Ringing command");
        };

        if !self.has_ready_to_answer() {
            let option = CallOption {
                recorder,
                ..Default::default()
            };
            let _ = self.invite_or_accept(option, "ringing".to_string()).await?;
        }

        if let Some(ready) = self.ready_to_answer.load_full() {
            let (headers, body) = if early_media.unwrap_or_default() || ringtone.is_some() {
                let headers = vec![rsipstack::rsip::Header::ContentType(
                    "application/sdp".to_string().into(),
                )];
                (Some(headers), Some(ready.answer.as_bytes().to_vec()))
            } else {
                (None, None)
            };

            ready.dialog.ringing(headers, body).ok();
            info!(
                session_id = self.session_id,
                ringtone, early_media, "playing ringtone"
            );
            if let Some(ringtone_url) = ringtone {
                self.do_play(Command::Play {
                    url: ringtone_url,
                    play_id: None,
                    auto_hangup: None,
                    wait_input_timeout: None,
                    offset_ms: None,
                })
                .await
                .ok();
            } else {
                info!(session_id = self.session_id, "no ringtone to play");
            }
        }
        Ok(())
    }

    async fn do_tts(&self, command: Command) -> Result<()> {
        let Command::Tts {
            text,
            speaker,
            play_id,
            auto_hangup,
            streaming,
            end_of_stream,
            option,
            wait_input_timeout,
            base64,
            cache_key,
        } = command
        else {
            unreachable!("do_tts called with non-Tts command");
        };
        let streaming = streaming.unwrap_or_default();
        let end_of_stream = end_of_stream.unwrap_or_default();
        let base64 = base64.unwrap_or_default();

        let tts_option = {
            let call_state = self.progress.load_full();
            match call_state.option.clone().unwrap_or_default().tts {
                Some(opt) => opt.merge_with(option),
                None => {
                    if let Some(opt) = option {
                        opt
                    } else {
                        return Err(anyhow::anyhow!("no tts option available"));
                    }
                }
            }
        };
        let speaker = match speaker {
            Some(s) => Some(s),
            None => tts_option.speaker.clone(),
        };

        let mut play_command = SynthesisCommand {
            text,
            speaker,
            play_id: play_id.clone(),
            streaming,
            end_of_stream: if !streaming { true } else { end_of_stream },
            option: tts_option,
            base64,
            cache_key,
            auto_hangup,
        };
        info!(
            session_id = self.session_id,
            provider = ?play_command.option.provider,
            text = %play_command.text.chars().take(10).collect::<String>(),
            speaker = play_command.speaker.as_deref(),
            auto_hangup = auto_hangup.unwrap_or_default(),
            play_id = play_command.play_id.as_deref(),
            streaming = play_command.streaming,
            end_of_stream = play_command.end_of_stream,
            wait_input_timeout = wait_input_timeout.unwrap_or_default(),
            is_base64 = play_command.base64,
            cache_key = play_command.cache_key.as_deref(),
            "new synthesis"
        );

        let ssrc = rand::random::<u32>();
        let (should_interrupt, picked_ssrc) = {
            let existing_handle = self.tts_handle.load_full();
            let current_play_id = self.current_play();

            let (target_ssrc, changed) = if let Some(handle) = &existing_handle {
                if play_id.is_some() && current_play_id != play_id {
                    (ssrc, true)
                } else {
                    (handle.ssrc, false)
                }
            } else {
                (ssrc, false)
            };

            // Defer auto_hangup setting until after potential interrupt.
            // auto_hangup will be set below after do_interrupt() to avoid being cleared.
            self.set_wait_input_timeout(wait_input_timeout);

            self.set_current_play(play_id.clone());
            (changed, target_ssrc)
        };

        if should_interrupt {
            let _ = self.do_interrupt(false).await;
        }

        // auto_hangup rides on the track: armed via `with_auto_hangup` when a
        // new track is created, or by the command itself for an existing track.

        let existing_handle = self.tts_handle.load_full();
        if let Some(tts_handle) = existing_handle {
            match tts_handle.try_send(play_command) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    play_command = e.0;
                }
            }
        }

        let (new_handle, tts_track) = StreamEngine::create_tts_track(
            self.app_state.stream_engine.clone(),
            self.cancel_token.child_token(),
            self.session_id.clone(),
            self.server_side_track_id.clone(),
            picked_ssrc,
            play_id.clone(),
            streaming,
            &play_command.option,
            play_command.auto_hangup,
        )
        .await?;

        new_handle.try_send(play_command)?;
        self.tts_handle.store(Some(Arc::new(new_handle)));
        self.update_track_wrapper(tts_track, play_id).await;
        Ok(())
    }

    async fn do_play(&self, command: Command) -> Result<()> {
        let Command::Play {
            url,
            play_id,
            auto_hangup,
            wait_input_timeout,
            offset_ms,
        } = command
        else {
            unreachable!("do_play called with non-Play command");
        };
        let ssrc = rand::random::<u32>();
        info!(
            session_id = self.session_id,
            ssrc, url, play_id, auto_hangup, "play file track"
        );

        let play_id = play_id.or(Some(url.clone()));

        // make_file_track uses the path as play_id; honor an explicit play_id here.
        let mut file_track = self
            .make_file_track(url, ssrc)
            .with_play_id(play_id.clone())
            .with_auto_hangup(auto_hangup);

        if let Some(offset) = offset_ms {
            file_track = file_track.with_offset_ms(offset);
        }

        {
            self.tts_handle.store(None);
            self.set_wait_input_timeout(wait_input_timeout);
        }

        self.update_track_wrapper(Box::new(file_track), play_id)
            .await;
        Ok(())
    }

    async fn do_history(&self, speaker: String, text: String) -> Result<()> {
        self.event_sender
            .send(SessionEvent::AddHistory {
                sender: Some(self.session_id.clone()),
                timestamp: crate::media::get_timestamp(),
                speaker,
                text,
            })
            .map(|_| ())
            .map_err(Into::into)
    }

    fn do_custom(&self, sender: Option<String>, data: serde_json::Value) -> Result<()> {
        self.event_sender
            .send(SessionEvent::Custom {
                timestamp: crate::media::get_timestamp(),
                sender,
                data,
            })
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn do_interrupt(&self, graceful: bool) -> Result<()> {
        {
            self.tts_handle.store(None);
            self.set_moh(None);
        }
        self.media_stream
            .remove_track(&self.server_side_track_id, graceful)
            .await;
        Ok(())
    }
    async fn do_pause(&self) -> Result<()> {
        self.media_stream
            .pause_playback(self.server_side_track_id.clone())
            .await?;
        Ok(())
    }
    async fn do_resume(&self) -> Result<()> {
        self.media_stream
            .resume_playback(self.server_side_track_id.clone())
            .await?;
        Ok(())
    }
    async fn do_hangup(
        &self,
        reason: Option<CallRecordHangupReason>,
        initiator: Option<String>,
        headers: Option<HashMap<String, String>>,
        refer: Option<bool>,
    ) -> Result<()> {
        info!(
            session_id = self.session_id,
            ?reason,
            ?initiator,
            ?headers,
            ?refer,
            "do_hangup"
        );

        let hangup_reason = match initiator.as_deref() {
            Some("caller") => CallRecordHangupReason::ByCaller,
            Some("callee") => CallRecordHangupReason::ByCallee,
            Some("system") => CallRecordHangupReason::Autohangup,
            _ => reason.unwrap_or(CallRecordHangupReason::BySystem),
        };

        match refer {
            Some(true) => {
                // Hang up only the refer call, leaving the main call alive.
                let refer_token = self.take_refer_call_token();
                let refer_leg = self.refer_leg_value();
                let has_refer_leg = refer_leg.is_some();
                if let Some(leg) = refer_leg {
                    if let Some(headers) = headers {
                        let h_val = serde_json::to_value(&headers).unwrap_or_default();
                        leg.set_extra("_hangup_headers", h_val);
                    }
                    // Set reason before cancelling so on_terminated() sees it.
                    let reason = hangup_reason.clone();
                    leg.update_progress(|p| p.set_hangup_reason(reason.clone()));
                }
                if let Some(token) = refer_token {
                    token.cancel();
                }
                if has_refer_leg {
                    self.media_stream
                        .remove_track(&self.server_side_track_id, false)
                        .await;
                }
            }
            _ => {
                if let Some(headers) = headers {
                    let h_val = serde_json::to_value(&headers).unwrap_or_default();
                    self.leg().set_extra("_hangup_headers", h_val);
                }
                self.leg()
                    .update_progress(|p| p.set_hangup_reason(hangup_reason.clone()));
                let refer_token = self.take_refer_call_token();
                self.media_stream
                    .stop(Some(hangup_reason.to_string()), initiator);
                if let Some(token) = refer_token {
                    token.cancel();
                }
            }
        }
        tokio::task::yield_now().await;
        Ok(())
    }

    /// Initiate a refer (attended transfer) leg.
    ///
    /// The INVITE handshake can take up to `timeout` seconds, so it runs in a
    /// spawned task and reports back through [`ActorMsg::ReferDone`]; the
    /// actor loop keeps serving events (e.g. MOH looping) meanwhile.
    async fn do_refer(
        &self,
        runtime: &mut CallRuntime,
        caller: String,
        callee: String,
        refer_option: Option<ReferOption>,
    ) -> Result<()> {
        self.do_interrupt(false).await.ok();

        // Check if we should pause parent ASR
        let pause_parent_asr = refer_option
            .as_ref()
            .and_then(|o| o.pause_parent_asr)
            .unwrap_or(false);

        // Save original ASR option for later resume
        let original_asr_option = if pause_parent_asr {
            self.progress
                .load_full()
                .option
                .as_ref()
                .and_then(|o| o.asr.clone())
        } else {
            None
        };

        // Pause parent ASR if requested
        if pause_parent_asr {
            info!(
                session_id = self.session_id,
                "Pausing parent call ASR during refer"
            );
            self.media_stream
                .remove_processor::<crate::media::asr_processor::AsrProcessor>(
                    &self.server_side_track_id,
                )
                .await
                .ok();
        }

        let mut moh = refer_option.as_ref().and_then(|o| o.moh.clone());
        if let Some(ref path) = moh {
            if !path.starts_with("http") && !std::path::Path::new(path).exists() {
                let fallback = "./config/sounds/refer_moh.wav";
                if std::path::Path::new(fallback).exists() {
                    info!(
                        session_id = self.session_id,
                        "moh {} not found, using fallback {}", path, fallback
                    );
                    moh = Some(fallback.to_string());
                }
            }
        }
        let ref_call_id = refer_option
            .as_ref()
            .and_then(|o| o.call_id.clone())
            .unwrap_or_else(|| format!("ref-{}-{}", rand::random::<u32>(), self.session_id));

        let session_id = self.session_id.clone();
        let track_id = self.server_side_track_id.clone();

        let (recorder, parent_caller) = {
            let progress = self.progress.load_full();
            let option = progress.option.as_ref();
            (
                option.map(|o| o.recorder.clone()).unwrap_or_default(),
                option.and_then(|o| o.caller.clone()),
            )
        };
        let caller = if caller.trim().is_empty() {
            parent_caller.unwrap_or_default()
        } else {
            caller
        };

        let mut call_option = CallOption {
            caller: Some(caller),
            callee: Some(callee.clone()),
            sip: refer_option.as_ref().and_then(|o| o.sip.clone()),
            vad: refer_option
                .as_ref()
                .and_then(|o| o.vad.clone())
                .map(|mut opts| {
                    opts.refer = Some(true);
                    opts
                }),
            asr: refer_option
                .as_ref()
                .and_then(|o| o.asr.clone())
                .map(|mut opts| {
                    opts.refer = Some(true);
                    opts
                }),
            denoise: refer_option.as_ref().and_then(|o| o.denoise.clone()),
            agc: refer_option.as_ref().and_then(|o| o.agc.clone()),
            recorder,
            ..Default::default()
        };
        call_option.check_default();

        let mut invite_option = call_option.build_invite_option()?;
        invite_option.call_id = Some(ref_call_id.clone());

        let headers = invite_option.headers.get_or_insert_with(|| Vec::new());

        {
            let progress = self.progress.load_full();
            if let Some(opt) = progress.option.as_ref() {
                if let Some(callee) = opt.callee.as_ref() {
                    headers.push(rsipstack::rsip::Header::Other(
                        "X-Referred-To".to_string(),
                        callee.clone(),
                    ));
                }
                if let Some(caller) = opt.caller.as_ref() {
                    headers.push(rsipstack::rsip::Header::Other(
                        "X-Referred-From".to_string(),
                        caller.clone(),
                    ));
                }
            }
        }

        headers.push(rsipstack::rsip::Header::Other(
            "X-Referred-Id".to_string(),
            self.session_id.clone(),
        ));

        let ssrc = rand::random::<u32>();
        let refer_leg = LegShared::new(
            ssrc,
            true,
            CallProgress {
                session_id: ref_call_id.clone(),
                start_time: Some(Utc::now()),
                option: Some(call_option.clone()),
                ..Default::default()
            },
        );
        self.set_refer_leg(Some(refer_leg.clone()));

        let auto_hangup_requested = refer_option
            .as_ref()
            .and_then(|o| o.auto_hangup)
            .unwrap_or(true);

        // auto_hangup rides on the refer leg's TrackEnd (InviteDialogStates).

        // Setup ASR resume after refer ends (if not auto_hangup and ASR was paused)
        if !auto_hangup_requested && pause_parent_asr && original_asr_option.is_some() {
            let asr_option = original_asr_option.unwrap();
            self.set_pending_asr_resume((ssrc, asr_option));
        }

        let timeout_secs = refer_option.as_ref().and_then(|o| o.timeout).unwrap_or(30);
        let forward_dtmf = refer_option
            .as_ref()
            .and_then(|o| o.forward_dtmf)
            .unwrap_or(true);

        info!(
            session_id = self.session_id,
            ssrc,
            auto_hangup = auto_hangup_requested,
            callee,
            timeout_secs,
            "do_refer"
        );

        let refer_cancel_token = self.cancel_token.child_token();
        self.set_refer_call_token(refer_cancel_token.clone());

        // Run the INVITE handshake in the background so the actor keeps
        // serving events (MOH looping, auto-hangup...) while it is in flight.
        let me = runtime
            .me
            .clone()
            .ok_or_else(|| anyhow::anyhow!("refer is only supported inside serve()"))?;
        let actor_tx = runtime.actor_tx.clone();
        let event_sender = self.event_sender.clone();
        let log_session_id = session_id.clone();
        let reject_track_id = track_id.clone();
        crate::spawn(async move {
            let out = crate::call::tracks::OutgoingLeg {
                cancel_token: refer_cancel_token,
                leg: refer_leg,
                track_id: track_id.clone(),
                invite_option,
                call_option,
                moh,
                auto_hangup: auto_hangup_requested,
            };
            let result = match tokio::time::timeout(
                Duration::from_secs(timeout_secs as u64),
                me.create_outgoing_sip_track(out),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => {
                    warn!(
                        session_id = log_session_id,
                        "refer sip track creation timed out after {} seconds", timeout_secs
                    );
                    event_sender
                        .send(SessionEvent::Reject {
                            track_id: reject_track_id,
                            timestamp: crate::media::get_timestamp(),
                            reason: "Timeout when refer".into(),
                            code: Some(408),
                            refer: Some(true),
                        })
                        .ok();
                    Err(rsipstack::Error::Error(
                        "refer sip track creation timed out".to_string(),
                    ))
                }
            };
            me.set_moh(None);
            actor_tx
                .send(ActorMsg::ReferDone {
                    track_id,
                    forward_dtmf,
                    result,
                })
                .await
                .ok();
        });

        Ok(())
    }

    async fn do_message(
        &self,
        body: String,
        content_type: Option<String>,
        headers: Option<HashMap<String, String>>,
        refer: Option<bool>,
    ) -> Result<()> {
        if !matches!(self.call_type, ActiveCallType::Sip | ActiveCallType::B2bua) {
            return Err(anyhow::anyhow!(
                "message command is only supported for SIP calls"
            ));
        }

        let dialog_key = if refer == Some(true) {
            self.refer_leg_value()
                .map(|leg| leg.progress.load_full().session_id.clone())
        } else {
            Some(self.progress.load_full().session_id.clone())
        };

        let mut dialog = dialog_key
            .as_ref()
            .filter(|id| !id.is_empty())
            .and_then(|id| self.invitation.dialog_layer.get_dialog_with(id));

        if dialog.is_none() {
            if let Some(target_id) = dialog_key.as_ref().filter(|id| !id.is_empty()) {
                dialog = self
                    .invitation
                    .dialog_layer
                    .all_dialog_ids()
                    .into_iter()
                    .filter_map(|id| self.invitation.dialog_layer.get_dialog_with(&id))
                    .find(|dialog| dialog.id().to_string() == *target_id);
            }
        }

        // Last resort: look up a confirmed client dialog by call id (the
        // dialog id for refer legs, the session id otherwise).
        if dialog.is_none() {
            let call_id = match (refer == Some(true), dialog_key.as_deref()) {
                (true, Some(id)) if !id.is_empty() => Some(id),
                (false, _) => Some(self.session_id.as_str()),
                _ => None,
            };
            if let Some(call_id) = call_id {
                dialog = self
                    .invitation
                    .dialog_layer
                    .get_client_dialog_by_call_id(call_id)
                    .into_iter()
                    .find(|d| {
                        matches!(
                            d.state(),
                            rsipstack::dialog::dialog::DialogState::Confirmed(_, _)
                        )
                    })
                    .map(rsipstack::dialog::dialog::Dialog::Invite);
            }
        }

        let dialog = dialog.ok_or_else(|| {
            anyhow::anyhow!(
                "no established SIP dialog found for message command, refer={}",
                refer.unwrap_or_default()
            )
        })?;

        let mut sip_headers = vec![rsipstack::rsip::Header::ContentType(
            content_type
                .clone()
                .unwrap_or_else(|| "text/plain;charset=utf-8".to_string())
                .into(),
        )];
        if let Some(headers) = &headers {
            sip_headers.extend(crate::sip_util::sip_headers_from_map(headers));
        }

        info!(
            session_id = self.session_id,
            dialog_id = %dialog.id(),
            content_type = content_type.as_deref().unwrap_or("text/plain;charset=utf-8"),
            refer = refer.unwrap_or_default(),
            body = %body.chars().take(64).collect::<String>(),
            "sending SIP MESSAGE"
        );

        let response = dialog
            .message(Some(sip_headers), Some(body.into_bytes()))
            .await?;
        match response {
            Some(resp)
                if resp.status_code.kind() == rsipstack::rsip::StatusCodeKind::Successful =>
            {
                Ok(())
            }
            Some(resp) => Err(anyhow::anyhow!(
                "SIP MESSAGE rejected with status {}",
                resp.status_code
            )),
            None => Err(anyhow::anyhow!(
                "SIP MESSAGE was not sent because dialog is not confirmed"
            )),
        }
    }

    fn bridge_track_id(source_session_id: &str, target_session_id: &str) -> TrackId {
        format!("bridge:{}:to:{}", source_session_id, target_session_id)
    }

    async fn do_bridge(&self, target_session_id: String) -> Result<()> {
        let target = {
            let calls = self.app_state.active_calls.lock().unwrap();
            calls.get(&target_session_id).cloned()
        };
        let target = target.ok_or_else(|| {
            anyhow::anyhow!("bridge target session not found: {}", target_session_id)
        })?;

        if target.session_id == self.session_id {
            return Err(anyhow::anyhow!("cannot bridge a call to itself").into());
        }

        let self_bridge_track_id = Self::bridge_track_id(&self.session_id, &target.session_id);
        let target_bridge_track_id = Self::bridge_track_id(&target.session_id, &self.session_id);

        self.media_stream
            .remove_track(&self_bridge_track_id, false)
            .await;
        target
            .media_stream
            .remove_track(&target_bridge_track_id, false)
            .await;

        let (self_bridge_sender, self_bridge_receiver) = mpsc::channel(25);
        let (target_bridge_sender, target_bridge_receiver) = mpsc::channel(25);

        let self_paused = self.bridge_paused.clone();
        let target_paused = target.bridge_paused.clone();

        let self_forwarding_track = ForwardingTrack::new(
            self_bridge_track_id.clone(),
            self.session_id.clone(),
            target_bridge_sender,
            self_bridge_receiver,
            self.track_config.clone(),
            self.cancel_token.child_token(),
            rand::random::<u32>(),
            self_paused,
        );

        let target_forwarding_track = ForwardingTrack::new(
            target_bridge_track_id.clone(),
            target.session_id.clone(),
            self_bridge_sender,
            target_bridge_receiver,
            target.track_config.clone(),
            target.cancel_token.child_token(),
            rand::random::<u32>(),
            target_paused,
        );

        self.media_stream
            .update_track(Box::new(self_forwarding_track), None)
            .await;
        target
            .media_stream
            .update_track(Box::new(target_forwarding_track), None)
            .await;

        info!(
            session_id = self.session_id,
            target = target_session_id,
            self_bridge_track_id,
            target_bridge_track_id,
            "audio bridge established"
        );
        Ok(())
    }

    async fn do_unbridge(&self, target_session_id: String) -> Result<()> {
        let target = {
            let calls = self.app_state.active_calls.lock().unwrap();
            calls.get(&target_session_id).cloned()
        };

        let self_bridge_track_id = Self::bridge_track_id(&self.session_id, &target_session_id);
        self.media_stream
            .remove_track(&self_bridge_track_id, false)
            .await;

        if let Some(target) = target {
            let target_bridge_track_id =
                Self::bridge_track_id(&target.session_id, &self.session_id);
            target
                .media_stream
                .remove_track(&target_bridge_track_id, false)
                .await;
            info!(
                session_id = self.session_id,
                target = target.session_id,
                self_bridge_track_id,
                target_bridge_track_id,
                "audio bridge removed"
            );
        } else {
            info!(
                session_id = self.session_id,
                target = target_session_id,
                self_bridge_track_id,
                "audio bridge removed locally; target session not active"
            );
        }

        Ok(())
    }

    async fn do_mute(&self, track_id: Option<String>) -> Result<()> {
        self.media_stream.mute_track(track_id).await;
        Ok(())
    }

    async fn do_unmute(&self, track_id: Option<String>) -> Result<()> {
        self.media_stream.unmute_track(track_id).await;
        Ok(())
    }

    pub async fn cleanup(&self) -> Result<()> {
        if matches!(self.call_type, ActiveCallType::Sip | ActiveCallType::B2bua) {
            self.reject_now(
                Some(rsipstack::rsip::StatusCode::Decline),
                Some("handler disconnected".to_string()),
            )
            .await;
        }
        // A prepared-but-unaccepted SIP answer (non-SIP call types) must not
        // leave the carrier leg ringing forever.
        if let Some(pending) = self.take_pending_sip_answer() {
            if let Ok(pending) = Arc::try_unwrap(pending) {
                pending
                    .dialog
                    .reject(
                        Some(rsipstack::rsip::StatusCode::Decline),
                        Some("handler disconnected".to_string()),
                    )
                    .ok();
                self.invitation
                    .dialog_layer
                    .remove_dialog(&pending.dialog.id());
            }
        }
        self.tts_handle.store(None);
        self.media_stream.cleanup().await.ok();
        Ok(())
    }

    /// Build the call record from lock-free snapshots; never blocks, so it is
    /// safe (and lossless) from synchronous `Drop`.
    pub fn get_callrecord(&self) -> Option<CallRecord> {
        let progress = self.progress.load_full();
        let extras = self.extras.load_full();
        let refer_leg = self.refer_leg_value();
        Some(build_callrecord(
            &progress,
            &extras,
            refer_leg.as_ref(),
            &self.app_state,
            self.session_id.clone(),
            self.call_type.clone(),
        ))
    }

    async fn dump_to_file(
        &self,
        dump_file: &mut File,
        cmd_receiver: &mut CommandReceiver,
        event_receiver: &mut EventReceiver,
    ) {
        loop {
            select! {
                _ = self.cancel_token.cancelled() => {
                    break;
                }
                Ok(cmd) = cmd_receiver.recv() => {
                    CallRecordEvent::write(CallRecordEventType::Command, cmd, dump_file)
                        .await;
                }
                Ok(event) = event_receiver.recv() => {
                    if matches!(event, SessionEvent::Binary{..}) {
                        continue;
                    }
                    CallRecordEvent::write(CallRecordEventType::Event, event, dump_file)
                        .await;
                }
            };
        }
    }

    async fn dump_loop(
        &self,
        dump_events: bool,
        mut dump_cmd_receiver: CommandReceiver,
        mut dump_event_receiver: EventReceiver,
    ) {
        if !dump_events {
            return;
        }

        let file_name = self.app_state.get_dump_events_file(&self.session_id);
        let mut dump_file = match File::options()
            .create(true)
            .append(true)
            .open(&file_name)
            .await
        {
            Ok(file) => file,
            Err(e) => {
                warn!(
                    session_id = self.session_id,
                    file_name, "failed to open dump events file: {}", e
                );
                return;
            }
        };
        self.dump_to_file(
            &mut dump_file,
            &mut dump_cmd_receiver,
            &mut dump_event_receiver,
        )
        .await;

        while let Ok(event) = dump_event_receiver.try_recv() {
            if matches!(event, SessionEvent::Binary { .. }) {
                continue;
            }
            CallRecordEvent::write(CallRecordEventType::Event, event, &mut dump_file).await;
        }
    }
}

/// Cancels the token on drop, guaranteeing shutdown on every exit path of the
/// actor loop (normal break, panic, or task abort).
struct CancelOnExit<'a>(&'a CancellationToken);

impl Drop for CancelOnExit<'_> {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        info!(session_id = self.session_id, "dropping active call");
        if let Some(sender) = self.app_state.callrecord_sender.as_ref() {
            if let Some(record) = self.get_callrecord() {
                if let Err(e) = sender.send(record) {
                    warn!(
                        session_id = self.session_id,
                        "failed to send call record: {}", e
                    );
                }
            }
        }
    }
}
