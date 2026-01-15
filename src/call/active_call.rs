use super::Command;
use crate::{
    CallOption, ReferOption,
    event::{EventReceiver, EventSender, SessionEvent},
    media::{
        TrackId,
        ambiance::AmbianceProcessor,
        engine::StreamEngine,
        negotiate::strip_ipv6_candidates,
        recorder::RecorderOption,
        stream::{MediaStream, MediaStreamBuilder},
        track::{
            Track, TrackConfig,
            file::FileTrack,
            media_pass::MediaPassTrack,
            rtc::{RtcTrack, RtcTrackConfig},
            tts::SynthesisHandle,
            websocket::{WebsocketBytesReceiver, WebsocketTrack},
        },
    },
    synthesis::{SynthesisCommand, SynthesisOption},
};
use crate::{
    app::AppState,
    call::{
        CommandReceiver, CommandSender,
        sip::{DialogStateReceiverGuard, Invitation, InviteDialogStates},
    },
    callrecord::{CallRecord, CallRecordEvent, CallRecordEventType, CallRecordHangupReason},
    useragent::invitation::PendingDialog,
};
use anyhow::Result;
use audio_codec::CodecType;
use chrono::{DateTime, Utc};
use rsipstack::dialog::{invitation::InviteOption, server_dialog::ServerInviteDialog};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{fs::File, select, sync::Mutex, sync::RwLock, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallParams {
    pub id: Option<String>,
    #[serde(rename = "dump")]
    pub dump_events: Option<bool>,
    #[serde(rename = "ping")]
    pub ping_interval: Option<u32>,
    pub server_side_track: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub enum ActiveCallType {
    Webrtc,
    B2bua,
    WebSocket,
    #[default]
    Sip,
}

#[derive(Default)]
pub struct ActiveCallState {
    pub session_id: String,
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub last_status_code: u16,
    pub option: Option<CallOption>,
    pub answer: Option<String>,
    pub ssrc: u32,
    pub refer_callstate: Option<ActiveCallStateRef>,
    pub extras: Option<HashMap<String, serde_json::Value>>,
    pub is_refer: bool,

    // Runtime state (migrated from ActiveCall to reduce multiple locks)
    pub tts_handle: Option<SynthesisHandle>,
    pub auto_hangup: Option<(u32, CallRecordHangupReason)>,
    pub wait_input_timeout: Option<u32>,
    pub moh: Option<String>,
    pub current_play_id: Option<String>,
    pub audio_receiver: Option<WebsocketBytesReceiver>,
    pub ready_to_answer: Option<(String, Option<Box<dyn Track>>, ServerInviteDialog)>,
}

pub type ActiveCallRef = Arc<ActiveCall>;
pub type ActiveCallStateRef = Arc<RwLock<ActiveCallState>>;

pub struct ActiveCall {
    pub call_state: ActiveCallStateRef,
    pub cancel_token: CancellationToken,
    pub call_type: ActiveCallType,
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub track_config: TrackConfig,
    pub event_sender: EventSender,
    pub app_state: AppState,
    pub invitation: Invitation,
    pub cmd_sender: CommandSender,
    pub dump_events: bool,
    pub server_side_track_id: TrackId,
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

impl ActiveCall {
    pub fn new(
        call_type: ActiveCallType,
        cancel_token: CancellationToken,
        session_id: String,
        invitation: Invitation,
        app_state: AppState,
        track_config: TrackConfig,
        audio_receiver: Option<WebsocketBytesReceiver>,
        dump_events: bool,
        server_side_track_id: Option<TrackId>,
        extras: Option<HashMap<String, serde_json::Value>>,
    ) -> Self {
        let event_sender = crate::event::create_event_sender();
        let cmd_sender = tokio::sync::broadcast::Sender::<Command>::new(32);
        let media_stream_builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.child_token());
        let media_stream = Arc::new(media_stream_builder.build());
        let call_state = Arc::new(RwLock::new(ActiveCallState {
            session_id: session_id.clone(),
            start_time: Utc::now(),
            ssrc: rand::random::<u32>(),
            extras,
            audio_receiver,
            ..Default::default()
        }));
        Self {
            cancel_token,
            call_type,
            session_id,
            call_state,
            media_stream,
            track_config,
            event_sender,
            app_state,
            invitation,
            cmd_sender,
            dump_events,
            server_side_track_id: server_side_track_id.unwrap_or("server-side-track".to_string()),
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

    pub async fn serve(&self, receiver: ActiveCallReceiver) -> Result<()> {
        let ActiveCallReceiver {
            mut cmd_receiver,
            dump_cmd_receiver,
            dump_event_receiver,
        } = receiver;

        let process_command_loop = async move {
            while let Ok(command) = cmd_receiver.recv().await {
                match self.dispatch(command).await {
                    Ok(_) => (),
                    Err(e) => {
                        warn!(session_id = self.session_id, "{}", e);
                        self.event_sender
                            .send(SessionEvent::Error {
                                track_id: self.session_id.clone(),
                                timestamp: crate::media::get_timestamp(),
                                sender: "command".to_string(),
                                error: e.to_string(),
                                code: None,
                            })
                            .ok();
                    }
                }
            }
        };
        self.app_state
            .total_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        tokio::join!(
            self.dump_loop(self.dump_events, dump_cmd_receiver, dump_event_receiver),
            async {
                select! {
                    _ = process_command_loop => {
                        info!(session_id = self.session_id, "command loop done");
                    }
                    _ = self.process() => {
                        info!(session_id = self.session_id, "call serve done");
                    }
                    _ = self.cancel_token.cancelled() => {
                        info!(session_id = self.session_id, "call cancelled - cleaning up resources");
                    }
                }
                self.cancel_token.cancel();
            }
        );
        Ok(())
    }

    async fn process(&self) -> Result<()> {
        let mut event_receiver = self.event_sender.subscribe();

        let input_timeout_expire = Arc::new(Mutex::new((0u64, 0u32)));
        let input_timeout_expire_ref = input_timeout_expire.clone();
        let event_sender = self.event_sender.clone();
        let wait_input_timeout_loop = async {
            loop {
                let (start_time, expire) = { *input_timeout_expire.lock().await };
                if expire > 0 && crate::media::get_timestamp() >= start_time + expire as u64 {
                    info!(session_id = self.session_id, "wait input timeout reached");
                    *input_timeout_expire.lock().await = (0, 0);
                    event_sender
                        .send(SessionEvent::Silence {
                            track_id: self.server_side_track_id.clone(),
                            timestamp: crate::media::get_timestamp(),
                            start_time,
                            duration: expire as u64,
                            samples: None,
                        })
                        .ok();
                }
                sleep(Duration::from_millis(100)).await;
            }
        };
        let server_side_track_id = self.server_side_track_id.clone();
        let event_hook_loop = async move {
            while let Ok(event) = event_receiver.recv().await {
                match event {
                    SessionEvent::Speaking { .. }
                    | SessionEvent::Dtmf { .. }
                    | SessionEvent::AsrDelta { .. }
                    | SessionEvent::AsrFinal { .. }
                    | SessionEvent::TrackStart { .. } => {
                        *input_timeout_expire_ref.lock().await = (0, 0);
                    }
                    SessionEvent::TrackEnd {
                        track_id,
                        play_id,
                        ssrc,
                        ..
                    } => {
                        if track_id != server_side_track_id {
                            continue;
                        }

                        let (moh_path, auto_hangup, wait_timeout_val) = {
                            let mut state = self.call_state.write().await;
                            if play_id != state.current_play_id {
                                debug!(
                                    session_id = self.session_id,
                                    ?play_id,
                                    current = ?state.current_play_id,
                                    "ignoring interrupted track end"
                                );
                                continue;
                            }
                            state.current_play_id = None;
                            (
                                state.moh.clone(),
                                state.auto_hangup.clone(),
                                state.wait_input_timeout.take(),
                            )
                        };

                        if let Some(path) = moh_path {
                            info!(session_id = self.session_id, "looping moh: {}", path);
                            let ssrc = rand::random::<u32>();
                            let file_track = FileTrack::new(self.server_side_track_id.clone())
                                .with_play_id(Some(path.clone()))
                                .with_ssrc(ssrc)
                                .with_path(path.clone())
                                .with_cancel_token(self.cancel_token.child_token());
                            self.update_track_wrapper(Box::new(file_track), Some(path))
                                .await;
                            continue;
                        }

                        if let Some((hangup_ssrc, hangup_reason)) = auto_hangup {
                            if hangup_ssrc == ssrc {
                                info!(
                                    session_id = self.session_id,
                                    ssrc, "auto hangup when track end track_id:{}", track_id
                                );
                                self.do_hangup(Some(hangup_reason), None).await.ok();
                            }
                        }

                        if let Some(timeout) = wait_timeout_val {
                            let expire = if timeout > 0 {
                                (crate::media::get_timestamp(), timeout)
                            } else {
                                (0, 0)
                            };
                            *input_timeout_expire_ref.lock().await = expire;
                        }
                    }
                    SessionEvent::Interrupt { receiver } => {
                        let track_id =
                            receiver.unwrap_or_else(|| self.server_side_track_id.clone());
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
                        self.do_hangup(Some(CallRecordHangupReason::InactivityTimeout), None)
                            .await
                            .ok();
                    }
                    SessionEvent::Error { track_id, .. } => {
                        if track_id != server_side_track_id {
                            continue;
                        }

                        let moh_info = {
                            let mut state = self.call_state.write().await;
                            if let Some(path) = state.moh.clone() {
                                let fallback = "./config/sounds/refer_moh.wav".to_string();
                                let next_path = if path != fallback
                                    && std::path::Path::new(&fallback).exists()
                                {
                                    info!(
                                        session_id = self.session_id,
                                        "moh error, switching to fallback: {}", fallback
                                    );
                                    state.moh = Some(fallback.clone());
                                    fallback
                                } else {
                                    info!(
                                        session_id = self.session_id,
                                        "looping moh on error: {}", path
                                    );
                                    path
                                };
                                Some(next_path)
                            } else {
                                None
                            }
                        };

                        if let Some(next_path) = moh_info {
                            let ssrc = rand::random::<u32>();
                            let file_track = FileTrack::new(self.server_side_track_id.clone())
                                .with_play_id(Some(next_path.clone()))
                                .with_ssrc(ssrc)
                                .with_path(next_path.clone())
                                .with_cancel_token(self.cancel_token.child_token());
                            self.update_track_wrapper(Box::new(file_track), Some(next_path))
                                .await;
                            continue;
                        }
                    }
                    _ => {}
                }
            }
        };

        select! {
            _ = wait_input_timeout_loop=>{
                info!(session_id = self.session_id, "wait input timeout loop done");
            }
            _ = self.media_stream.serve() => {
                info!(session_id = self.session_id, "media stream loop done");
            }
            _ = event_hook_loop => {
                info!(session_id = self.session_id, "event loop done");
            }
        }
        Ok(())
    }

    async fn dispatch(&self, command: Command) -> Result<()> {
        match command {
            Command::Invite { option } => self.do_invite(option).await,
            Command::Accept { option } => self.do_accept(option).await,
            Command::Reject { reason, code } => {
                self.do_reject(code.map(|c| (c as u16).into()), Some(reason))
                    .await
            }
            Command::Ringing {
                ringtone,
                recorder,
                early_media,
            } => self.do_ringing(ringtone, recorder, early_media).await,
            Command::Tts {
                text,
                speaker,
                play_id,
                auto_hangup,
                streaming,
                end_of_stream,
                option,
                wait_input_timeout,
                base64,
            } => {
                self.do_tts(
                    text,
                    speaker,
                    play_id,
                    auto_hangup,
                    streaming.unwrap_or_default(),
                    end_of_stream.unwrap_or_default(),
                    option,
                    wait_input_timeout,
                    base64.unwrap_or_default(),
                )
                .await
            }
            Command::Play {
                url,
                play_id,
                auto_hangup,
                wait_input_timeout,
            } => {
                self.do_play(url, play_id, auto_hangup, wait_input_timeout)
                    .await
            }
            Command::Hangup { reason, initiator } => {
                let reason = reason.map(|r| {
                    r.parse::<CallRecordHangupReason>()
                        .unwrap_or(CallRecordHangupReason::BySystem)
                });
                self.do_hangup(reason, initiator).await
            }
            Command::Refer {
                caller,
                callee,
                options,
            } => self.do_refer(caller, callee, options).await,
            Command::Mute { track_id } => self.do_mute(track_id).await,
            Command::Unmute { track_id } => self.do_unmute(track_id).await,
            Command::Pause {} => self.do_pause().await,
            Command::Resume {} => self.do_resume().await,
            Command::Interrupt {
                graceful: passage,
                fade_out_ms: _,
            } => self.do_interrupt(passage.unwrap_or_default()).await,
            Command::History { speaker, text } => self.do_history(speaker, text).await,
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
            let state = self.call_state.read().await;
            option = state.merge_option(option);
        }

        option.check_default();
        if let Some(opt) = self.build_record_option(&option) {
            self.media_stream.update_recorder_option(opt).await;
        }

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
                self.do_hangup(Some(CallRecordHangupReason::BySystem), None)
                    .await
                    .ok();
                return Err(e);
            }
        }
    }

    async fn do_invite(&self, option: CallOption) -> Result<()> {
        self.invite_or_accept(option, "invite".to_string())
            .await
            .map(|_| ())
    }

    async fn do_accept(&self, mut option: CallOption) -> Result<()> {
        let has_pending = self.invitation.has_pending_call(&self.session_id).is_some();
        let ready_to_answer_val = {
            let state = self.call_state.read().await;
            state.ready_to_answer.is_none()
        };

        if ready_to_answer_val {
            if !has_pending {
                return Err(anyhow::anyhow!("no pending call to accept"));
            }
            option = self.invite_or_accept(option, "accept".to_string()).await?;
        } else {
            option.check_default();
            self.call_state.write().await.option = Some(option.clone());
        }

        let ready = self.call_state.write().await.ready_to_answer.take();
        if let Some((answer, track, dialog)) = ready {
            info!(
                session_id = self.session_id,
                track_id = track.as_ref().map(|t| t.id()),
                "ready to answer with track"
            );

            let headers = vec![rsip::Header::ContentType(
                "application/sdp".to_string().into(),
            )];

            match dialog.accept(Some(headers), Some(answer.as_bytes().to_vec())) {
                Ok(_) => {
                    {
                        let mut state = self.call_state.write().await;
                        state.answer = Some(answer);
                        state.answer_time = Some(Utc::now());
                    }
                    self.finish_caller_stack(&option, track).await?;
                }
                Err(e) => {
                    warn!(session_id = self.session_id, "failed to accept call: {}", e);
                    return Err(anyhow::anyhow!("failed to accept call"));
                }
            }
        }
        return Ok(());
    }

    async fn do_reject(
        &self,
        code: Option<rsip::StatusCode>,
        reason: Option<String>,
    ) -> Result<()> {
        match self.invitation.has_pending_call(&self.session_id) {
            Some(id) => {
                info!(
                    session_id = self.session_id,
                    ?reason,
                    ?code,
                    "rejecting call"
                );
                self.invitation.hangup(id, code, reason).await
            }
            None => Ok(()),
        }
    }

    async fn do_ringing(
        &self,
        ringtone: Option<String>,
        recorder: Option<RecorderOption>,
        early_media: Option<bool>,
    ) -> Result<()> {
        let ready_to_answer_val = self.call_state.read().await.ready_to_answer.is_none();
        if ready_to_answer_val {
            let option = CallOption {
                recorder,
                ..Default::default()
            };
            let _ = self.invite_or_accept(option, "ringing".to_string()).await?;
        }

        let state = self.call_state.read().await;
        if let Some((answer, _, dialog)) = state.ready_to_answer.as_ref() {
            let (headers, body) = if early_media.unwrap_or_default() || ringtone.is_some() {
                let headers = vec![rsip::Header::ContentType(
                    "application/sdp".to_string().into(),
                )];
                (Some(headers), Some(answer.as_bytes().to_vec()))
            } else {
                (None, None)
            };

            dialog.ringing(headers, body).ok();
            info!(
                session_id = self.session_id,
                ringtone, early_media, "playing ringtone"
            );
            if let Some(ringtone_url) = ringtone {
                drop(state);
                self.do_play(ringtone_url, None, None, None).await.ok();
            } else {
                info!(session_id = self.session_id, "no ringtone to play");
            }
        }
        Ok(())
    }

    async fn do_tts(
        &self,
        text: String,
        speaker: Option<String>,
        play_id: Option<String>,
        auto_hangup: Option<bool>,
        streaming: bool,
        end_of_stream: bool,
        option: Option<SynthesisOption>,
        wait_input_timeout: Option<u32>,
        base64: bool,
    ) -> Result<()> {
        let tts_option = {
            let call_state = self.call_state.read().await;
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
            end_of_stream,
            option: tts_option,
            base64,
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
            "new synthesis"
        );

        let ssrc = rand::random::<u32>();
        let should_interrupt = {
            let mut state = self.call_state.write().await;
            state.auto_hangup = match auto_hangup {
                Some(true) => Some((ssrc, CallRecordHangupReason::BySystem)),
                _ => None,
            };
            state.wait_input_timeout = wait_input_timeout;

            let changed = play_id.is_some() && state.current_play_id != play_id;
            state.current_play_id = play_id.clone();
            changed
        };

        if should_interrupt {
            let _ = self.do_interrupt(false).await;
        }

        let existing_handle = self.call_state.read().await.tts_handle.clone();
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
            ssrc,
            play_id.clone(),
            streaming,
            &play_command.option,
        )
        .await?;

        new_handle.try_send(play_command)?;
        self.call_state.write().await.tts_handle = Some(new_handle);
        self.update_track_wrapper(tts_track, play_id).await;
        Ok(())
    }

    async fn do_play(
        &self,
        url: String,
        play_id: Option<String>,
        auto_hangup: Option<bool>,
        wait_input_timeout: Option<u32>,
    ) -> Result<()> {
        let ssrc = rand::random::<u32>();
        info!(
            session_id = self.session_id,
            ssrc, url, play_id, auto_hangup, "play file track"
        );

        let play_id = play_id.or(Some(url.clone()));

        let file_track = FileTrack::new(self.server_side_track_id.clone())
            .with_play_id(play_id.clone())
            .with_ssrc(ssrc)
            .with_path(url)
            .with_cancel_token(self.cancel_token.child_token());

        {
            let mut state = self.call_state.write().await;
            state.tts_handle = None;
            state.auto_hangup = match auto_hangup {
                Some(true) => Some((ssrc, CallRecordHangupReason::BySystem)),
                _ => None,
            };
            state.wait_input_timeout = wait_input_timeout;
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

    async fn do_interrupt(&self, graceful: bool) -> Result<()> {
        {
            let mut state = self.call_state.write().await;
            state.tts_handle = None;
            state.moh = None;
        }
        self.media_stream
            .remove_track(&self.server_side_track_id, graceful)
            .await;
        Ok(())
    }
    async fn do_pause(&self) -> Result<()> {
        Ok(())
    }
    async fn do_resume(&self) -> Result<()> {
        Ok(())
    }
    async fn do_hangup(
        &self,
        reason: Option<CallRecordHangupReason>,
        initiator: Option<String>,
    ) -> Result<()> {
        info!(
            session_id = self.session_id,
            ?reason,
            ?initiator,
            "do_hangup"
        );

        // Set hangup reason based on initiator and reason
        let hangup_reason = match initiator.as_deref() {
            Some("caller") => CallRecordHangupReason::ByCaller,
            Some("callee") => CallRecordHangupReason::ByCallee,
            Some("system") => CallRecordHangupReason::Autohangup,
            _ => reason.unwrap_or(CallRecordHangupReason::BySystem),
        };

        self.media_stream
            .stop(Some(hangup_reason.to_string()), initiator);

        self.call_state
            .write()
            .await
            .set_hangup_reason(hangup_reason);
        Ok(())
    }

    async fn do_refer(
        &self,
        caller: String,
        callee: String,
        refer_option: Option<ReferOption>,
    ) -> Result<()> {
        self.do_interrupt(false).await.ok();
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
        let session_id = self.session_id.clone();
        let track_id = self.server_side_track_id.clone();

        let recorder = {
            let cs = self.call_state.read().await;
            cs.option
                .as_ref()
                .map(|o| o.recorder.clone())
                .unwrap_or_default()
        };

        let call_option = CallOption {
            caller: Some(caller),
            callee: Some(callee.clone()),
            sip: refer_option.as_ref().and_then(|o| o.sip.clone()),
            asr: refer_option.as_ref().and_then(|o| o.asr.clone()),
            denoise: refer_option.as_ref().and_then(|o| o.denoise.clone()),
            recorder,
            ..Default::default()
        };

        let mut invite_option = call_option.build_invite_option()?;
        invite_option.call_id = Some(self.session_id.clone());

        let headers = invite_option.headers.get_or_insert_with(|| Vec::new());

        {
            let cs = self.call_state.read().await;
            if let Some(opt) = cs.option.as_ref() {
                if let Some(callee) = opt.callee.as_ref() {
                    headers.push(rsip::Header::Other(
                        "X-Referred-To".to_string(),
                        callee.clone(),
                    ));
                }
                if let Some(caller) = opt.caller.as_ref() {
                    headers.push(rsip::Header::Other(
                        "X-Referred-From".to_string(),
                        caller.clone(),
                    ));
                }
            }
        }

        headers.push(rsip::Header::Other(
            "X-Referred-Id".to_string(),
            self.session_id.clone(),
        ));

        let ssrc = rand::random::<u32>();
        let refer_call_state = Arc::new(RwLock::new(ActiveCallState {
            start_time: Utc::now(),
            ssrc,
            option: Some(call_option.clone()),
            is_refer: true,
            ..Default::default()
        }));

        {
            let mut cs = self.call_state.write().await;
            cs.refer_callstate.replace(refer_call_state.clone());
        }

        let auto_hangup_requested = refer_option
            .as_ref()
            .and_then(|o| o.auto_hangup)
            .unwrap_or(true);

        if auto_hangup_requested {
            self.call_state.write().await.auto_hangup =
                Some((ssrc, CallRecordHangupReason::ByRefer));
        } else {
            self.call_state.write().await.auto_hangup = None;
        }

        let timeout_secs = refer_option.as_ref().and_then(|o| o.timeout).unwrap_or(30);

        info!(
            session_id = self.session_id,
            ssrc,
            auto_hangup = auto_hangup_requested,
            callee,
            timeout_secs,
            "do_refer"
        );

        let r = tokio::time::timeout(
            Duration::from_secs(timeout_secs as u64),
            self.create_outgoing_sip_track(
                self.cancel_token.child_token(),
                refer_call_state.clone(),
                &track_id,
                invite_option,
                &call_option,
                moh,
                auto_hangup_requested,
            ),
        )
        .await;

        {
            self.call_state.write().await.moh = None;
        }

        let result = match r {
            Ok(res) => res,
            Err(_) => {
                warn!(
                    session_id = session_id,
                    "refer sip track creation timed out after {} seconds", timeout_secs
                );
                self.event_sender
                    .send(SessionEvent::Reject {
                        track_id,
                        timestamp: crate::media::get_timestamp(),
                        reason: "Timeout when refer".into(),
                        code: Some(408),
                        refer: Some(true),
                    })
                    .ok();
                return Err(anyhow::anyhow!("refer sip track creation timed out").into());
            }
        };

        match result {
            Ok(answer) => {
                self.event_sender
                    .send(SessionEvent::Answer {
                        timestamp: crate::media::get_timestamp(),
                        track_id,
                        sdp: answer,
                        refer: Some(true),
                    })
                    .ok();
            }
            Err(e) => {
                warn!(
                    session_id = session_id,
                    "failed to create refer sip track: {}", e
                );
                match &e {
                    rsipstack::Error::DialogError(reason, _, code) => {
                        self.event_sender
                            .send(SessionEvent::Reject {
                                track_id,
                                timestamp: crate::media::get_timestamp(),
                                reason: reason.clone(),
                                code: Some(code.code() as u32),
                                refer: Some(true),
                            })
                            .ok();
                    }
                    _ => {}
                }
                return Err(e.into());
            }
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
        self.call_state.write().await.tts_handle = None;
        self.media_stream.cleanup().await.ok();
        Ok(())
    }

    pub fn get_callrecord(&self) -> Option<CallRecord> {
        self.call_state.try_read().ok().map(|call_state| {
            call_state.build_callrecord(
                self.app_state.clone(),
                self.session_id.clone(),
                self.call_type.clone(),
            )
        })
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

    pub async fn create_rtp_track(&self, track_id: TrackId, ssrc: u32) -> Result<RtcTrack> {
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = rustrtc::TransportMode::Rtp;

        if let Some(codecs) = &self.app_state.config.codecs {
            let mut codec_types = Vec::new();
            for c in codecs {
                match c.to_lowercase().as_str() {
                    "pcmu" => codec_types.push(CodecType::PCMU),
                    "pcma" => codec_types.push(CodecType::PCMA),
                    "g722" => codec_types.push(CodecType::G722),
                    "g729" => codec_types.push(CodecType::G729),
                    "opus" => codec_types.push(CodecType::Opus),
                    "dtmf" | "2833" | "telephone_event" => {
                        codec_types.push(CodecType::TelephoneEvent)
                    }
                    _ => {}
                }
            }
            if !codec_types.is_empty() {
                rtc_config.preferred_codec = Some(codec_types[0].clone());
                rtc_config.codecs = codec_types;
            }
        }

        if rtc_config.preferred_codec.is_none() {
            rtc_config.preferred_codec = Some(self.track_config.codec.clone());
        }

        rtc_config.rtp_port_range = self
            .app_state
            .config
            .rtp_start_port
            .zip(self.app_state.config.rtp_end_port);

        if let Some(ref external_ip) = self.app_state.config.external_ip {
            rtc_config.external_ip = Some(external_ip.clone());
        }

        let mut track = RtcTrack::new(
            self.cancel_token.child_token(),
            track_id,
            self.track_config.clone(),
            rtc_config,
        )
        .with_ssrc(ssrc);

        track.create().await?;

        Ok(track)
    }

    async fn setup_caller_track(&self, option: &CallOption) -> Result<()> {
        self.call_state.write().await.option = Some(option.clone());
        info!(
            session_id = self.session_id,
            call_type = ?self.call_type,
            "setup caller track"
        );

        let track = match self.call_type {
            ActiveCallType::Webrtc => Some(self.create_webrtc_track().await?),
            ActiveCallType::WebSocket => {
                let audio_receiver = self.call_state.write().await.audio_receiver.take();
                if let Some(receiver) = audio_receiver {
                    Some(self.create_websocket_track(receiver).await?)
                } else {
                    None
                }
            }
            ActiveCallType::Sip => {
                if let Some(pending_dialog) = self.invitation.get_pending_call(&self.session_id) {
                    return self
                        .prepare_incoming_sip_track(
                            self.cancel_token.clone(),
                            self.call_state.clone(),
                            &self.session_id,
                            pending_dialog,
                        )
                        .await;
                }

                let invite_option = option.build_invite_option()?;
                match self
                    .create_outgoing_sip_track(
                        self.cancel_token.clone(),
                        self.call_state.clone(),
                        &self.session_id,
                        invite_option,
                        &option,
                        None,
                        false,
                    )
                    .await
                {
                    Ok(answer) => {
                        self.event_sender
                            .send(SessionEvent::Answer {
                                timestamp: crate::media::get_timestamp(),
                                track_id: self.session_id.clone(),
                                sdp: answer,
                                refer: Some(false),
                            })
                            .ok();
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            session_id = self.session_id,
                            "failed to create sip track: {}", e
                        );
                        match &e {
                            rsipstack::Error::DialogError(reason, _, code) => {
                                self.event_sender
                                    .send(SessionEvent::Reject {
                                        track_id: self.session_id.clone(),
                                        timestamp: crate::media::get_timestamp(),
                                        reason: reason.clone(),
                                        code: Some(code.code() as u32),
                                        refer: Some(false),
                                    })
                                    .ok();
                            }
                            _ => {}
                        }
                        return Err(e.into());
                    }
                }
            }
            ActiveCallType::B2bua => match self.invitation.get_pending_call(&self.session_id) {
                Some(pending_dialog) => {
                    return self
                        .prepare_incoming_sip_track(
                            self.cancel_token.clone(),
                            self.call_state.clone(),
                            &self.session_id,
                            pending_dialog,
                        )
                        .await;
                }
                None => {
                    warn!(
                        session_id = self.session_id,
                        "no pending dialog found for B2BUA call"
                    );
                    return Err(anyhow::anyhow!(
                        "no pending dialog found for session_id: {}",
                        self.session_id
                    ));
                }
            },
        };
        match track {
            Some(track) => {
                self.finish_caller_stack(&option, Some(track)).await?;
            }
            None => {
                warn!(session_id = self.session_id, "no track created for caller");
                return Err(anyhow::anyhow!("no track created for caller"));
            }
        }
        Ok(())
    }

    async fn finish_caller_stack(
        &self,
        option: &CallOption,
        track: Option<Box<dyn Track>>,
    ) -> Result<()> {
        if let Some(track) = track {
            self.setup_track_with_stream(&option, track).await?;
        }

        {
            let call_state = self.call_state.read().await;
            if let Some(ref answer) = call_state.answer {
                info!(
                    session_id = self.session_id,
                    "sending answer event: {}", answer,
                );
                self.event_sender
                    .send(SessionEvent::Answer {
                        timestamp: crate::media::get_timestamp(),
                        track_id: self.session_id.clone(),
                        sdp: answer.clone(),
                        refer: Some(false),
                    })
                    .ok();
            } else {
                warn!(
                    session_id = self.session_id,
                    "no answer in state to send event"
                );
            }
        }
        Ok(())
    }

    pub async fn setup_track_with_stream(
        &self,
        option: &CallOption,
        mut track: Box<dyn Track>,
    ) -> Result<()> {
        let processors = match StreamEngine::create_processors(
            self.app_state.stream_engine.clone(),
            track.as_ref(),
            self.cancel_token.child_token(),
            self.event_sender.clone(),
            self.media_stream.packet_sender.clone(),
            option,
        )
        .await
        {
            Ok(processors) => processors,
            Err(e) => {
                warn!(
                    session_id = self.session_id,
                    "failed to prepare stream processors: {}", e
                );
                vec![]
            }
        };

        // Add all processors from the hook
        for processor in processors {
            track.append_processor(processor);
        }

        self.update_track_wrapper(track, None).await;
        Ok(())
    }

    pub async fn update_track_wrapper(&self, mut track: Box<dyn Track>, play_id: Option<String>) {
        let ambiance_opt = {
            let state = self.call_state.read().await;
            let mut opt = state
                .option
                .as_ref()
                .and_then(|o| o.ambiance.clone())
                .unwrap_or_default();

            if let Some(global) = &self.app_state.config.ambiance {
                opt.merge(global);
            }
            opt
        };
        if track.id() == &self.server_side_track_id && ambiance_opt.path.is_some() {
            match AmbianceProcessor::new(ambiance_opt).await {
                Ok(ambiance) => {
                    info!(session_id = self.session_id, "loaded ambiance processor");
                    track.append_processor(Box::new(ambiance));
                }
                Err(e) => {
                    tracing::error!("failed to load ambiance wav {}", e);
                }
            }
        }
        self.call_state.write().await.current_play_id = play_id.clone();
        self.media_stream.update_track(track, play_id).await;
    }

    pub async fn create_websocket_track(
        &self,
        audio_receiver: WebsocketBytesReceiver,
    ) -> Result<Box<dyn Track>> {
        let (ssrc, codec) = {
            let call_state = self.call_state.read().await;
            (
                call_state.ssrc,
                call_state
                    .option
                    .as_ref()
                    .map(|o| o.codec.clone())
                    .unwrap_or_default(),
            )
        };

        let ws_track = WebsocketTrack::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            self.track_config.clone(),
            self.event_sender.clone(),
            audio_receiver,
            codec,
            ssrc,
        );

        {
            let mut call_state = self.call_state.write().await;
            call_state.answer_time = Some(Utc::now());
            call_state.answer = Some("".to_string());
            call_state.last_status_code = 200;
        }

        Ok(Box::new(ws_track))
    }

    pub(super) async fn create_webrtc_track(&self) -> Result<Box<dyn Track>> {
        let (ssrc, option) = {
            let call_state = self.call_state.read().await;
            (
                call_state.ssrc,
                call_state.option.clone().unwrap_or_default(),
            )
        };

        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = rustrtc::TransportMode::WebRtc; // WebRTC
        rtc_config.ice_servers = self.app_state.config.ice_servers.clone();

        if let Some(codecs) = &self.app_state.config.codecs {
            let mut codec_types = Vec::new();
            for c in codecs {
                match c.to_lowercase().as_str() {
                    "pcmu" => codec_types.push(CodecType::PCMU),
                    "pcma" => codec_types.push(CodecType::PCMA),
                    "g722" => codec_types.push(CodecType::G722),
                    "g729" => codec_types.push(CodecType::G729),
                    "opus" => codec_types.push(CodecType::Opus),
                    "dtmf" | "2833" | "telephone_event" => {
                        codec_types.push(CodecType::TelephoneEvent)
                    }
                    _ => {}
                }
            }
            if !codec_types.is_empty() {
                rtc_config.preferred_codec = Some(codec_types[0].clone());
                rtc_config.codecs = codec_types;
            }
        }

        if let Some(ref external_ip) = self.app_state.config.external_ip {
            rtc_config.external_ip = Some(external_ip.clone());
        }

        let mut webrtc_track = RtcTrack::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            self.track_config.clone(),
            rtc_config,
        )
        .with_ssrc(ssrc);

        let timeout = option.handshake_timeout.map(|t| Duration::from_secs(t));
        let offer = match option.enable_ipv6 {
            Some(false) | None => {
                strip_ipv6_candidates(option.offer.as_ref().unwrap_or(&"".to_string()))
            }
            _ => option.offer.clone().unwrap_or("".to_string()),
        };
        let answer: Option<String>;
        match webrtc_track.handshake(offer, timeout).await {
            Ok(answer_sdp) => {
                answer = match option.enable_ipv6 {
                    Some(false) | None => Some(strip_ipv6_candidates(&answer_sdp)),
                    Some(true) => Some(answer_sdp.to_string()),
                };
            }
            Err(e) => {
                warn!(session_id = self.session_id, "failed to setup track: {}", e);
                return Err(anyhow::anyhow!("Failed to setup track: {}", e));
            }
        }

        {
            let mut call_state = self.call_state.write().await;
            call_state.answer_time = Some(Utc::now());
            call_state.answer = answer;
            call_state.last_status_code = 200;
        }
        Ok(Box::new(webrtc_track))
    }

    async fn create_outgoing_sip_track(
        &self,
        cancel_token: CancellationToken,
        call_state_ref: ActiveCallStateRef,
        track_id: &String,
        mut invite_option: InviteOption,
        call_option: &CallOption,
        moh: Option<String>,
        auto_hangup: bool,
    ) -> Result<String, rsipstack::Error> {
        let ssrc = call_state_ref.read().await.ssrc;
        let rtp_track = self
            .create_rtp_track(track_id.clone(), ssrc)
            .await
            .map_err(|e| rsipstack::Error::Error(e.to_string()))?;

        let offer = Some(
            rtp_track
                .local_description()
                .await
                .map_err(|e| rsipstack::Error::Error(e.to_string()))?,
        );

        {
            let mut cs = call_state_ref.write().await;
            if let Some(o) = cs.option.as_mut() {
                o.offer = offer.clone();
            }
            cs.start_time = Utc::now();
        };

        invite_option.offer = offer.clone().map(|s| s.into());

        let mut rtp_track_to_setup = Some(Box::new(rtp_track) as Box<dyn Track>);

        if let Some(moh) = moh {
            let ssrc_and_moh = {
                let mut state = call_state_ref.write().await;
                state.moh = Some(moh.clone());
                if state.current_play_id.is_none() {
                    let ssrc = rand::random::<u32>();
                    Some((ssrc, moh.clone()))
                } else {
                    info!(
                        session_id = self.session_id,
                        "Something is playing, MOH will start after it ends"
                    );
                    None
                }
            };

            if let Some((ssrc, moh_path)) = ssrc_and_moh {
                let file_track = FileTrack::new(self.server_side_track_id.clone())
                    .with_play_id(Some(moh_path.clone()))
                    .with_ssrc(ssrc)
                    .with_path(moh_path.clone())
                    .with_cancel_token(self.cancel_token.child_token());
                self.update_track_wrapper(Box::new(file_track), Some(moh_path))
                    .await;
            }
        } else {
            let track = rtp_track_to_setup.take().unwrap();
            self.setup_track_with_stream(&call_option, track)
                .await
                .map_err(|e| rsipstack::Error::Error(e.to_string()))?;
        }

        info!(
            session_id = self.session_id,
            track_id,
            contact = %invite_option.contact,
            "invite {} -> {} offer: \n{}",
            invite_option.caller,
            invite_option.callee,
            offer.as_ref().map(|s| s.as_str()).unwrap_or("<NO OFFER>")
        );

        let (dlg_state_sender, dlg_state_receiver) =
            self.invitation.dialog_layer.new_dialog_state_channel();

        let states = InviteDialogStates {
            is_client: true,
            session_id: self.session_id.clone(),
            track_id: track_id.clone(),
            event_sender: self.event_sender.clone(),
            media_stream: self.media_stream.clone(),
            call_state: call_state_ref.clone(),
            cancel_token,
            terminated_reason: None,
        };

        let mut client_dialog_handler =
            DialogStateReceiverGuard::new(self.invitation.dialog_layer.clone(), dlg_state_receiver);

        crate::spawn(async move {
            client_dialog_handler.process_dialog(states).await;
        });

        let (dialog_id, answer) = self
            .invitation
            .invite(invite_option, dlg_state_sender)
            .await?;

        self.call_state.write().await.moh = None;

        if let Some(track) = rtp_track_to_setup {
            self.setup_track_with_stream(&call_option, track)
                .await
                .map_err(|e| rsipstack::Error::Error(e.to_string()))?;
        }

        let answer = match answer {
            Some(answer) => String::from_utf8_lossy(&answer).to_string(),
            None => {
                warn!(session_id = self.session_id, "no answer received");
                return Err(rsipstack::Error::DialogError(
                    "No answer received".to_string(),
                    dialog_id,
                    rsip::StatusCode::NotAcceptableHere,
                ));
            }
        };

        {
            let mut cs = call_state_ref.write().await;
            if cs.answer.is_none() {
                cs.answer = Some(answer.clone());
            }
            if auto_hangup {
                cs.auto_hangup = Some((ssrc, CallRecordHangupReason::ByRefer));
            }
        }

        self.media_stream
            .update_remote_description(&track_id, &answer)
            .await
            .ok();

        Ok(answer)
    }

    /// Detect if SDP is WebRTC format
    pub fn is_webrtc_sdp(sdp: &str) -> bool {
        (sdp.contains("a=ice-ufrag:") || sdp.contains("a=ice-pwd:"))
            && sdp.contains("a=fingerprint:")
    }

    pub async fn setup_answer_track(
        &self,
        ssrc: u32,
        option: &CallOption,
        offer: String,
    ) -> Result<(String, Box<dyn Track>)> {
        let offer = match option.enable_ipv6 {
            Some(false) | None => strip_ipv6_candidates(&offer),
            _ => offer.clone(),
        };

        let timeout = option.handshake_timeout.map(|t| Duration::from_secs(t));

        let mut media_track = if Self::is_webrtc_sdp(&offer) {
            let mut rtc_config = RtcTrackConfig::default();
            rtc_config.mode = rustrtc::TransportMode::WebRtc;
            rtc_config.ice_servers = self.app_state.config.ice_servers.clone();
            if let Some(ref external_ip) = self.app_state.config.external_ip {
                rtc_config.external_ip = Some(external_ip.clone());
            }

            let webrtc_track = RtcTrack::new(
                self.cancel_token.child_token(),
                self.session_id.clone(),
                self.track_config.clone(),
                rtc_config,
            )
            .with_ssrc(ssrc);

            Box::new(webrtc_track) as Box<dyn Track>
        } else {
            let rtp_track = self.create_rtp_track(self.session_id.clone(), ssrc).await?;
            Box::new(rtp_track) as Box<dyn Track>
        };

        let answer = match media_track.handshake(offer.clone(), timeout).await {
            Ok(answer) => answer,
            Err(e) => {
                return Err(anyhow::anyhow!("handshake failed: {e}"));
            }
        };

        return Ok((answer, media_track));
    }

    pub async fn prepare_incoming_sip_track(
        &self,
        cancel_token: CancellationToken,
        call_state_ref: ActiveCallStateRef,
        track_id: &String,
        pending_dialog: PendingDialog,
    ) -> Result<()> {
        let state_receiver = pending_dialog.state_receiver;
        //let pending_token_clone = pending_dialog.token;

        let states = InviteDialogStates {
            is_client: false,
            session_id: self.session_id.clone(),
            track_id: track_id.clone(),
            event_sender: self.event_sender.clone(),
            media_stream: self.media_stream.clone(),
            call_state: self.call_state.clone(),
            cancel_token,
            terminated_reason: None,
        };

        let initial_request = pending_dialog.dialog.initial_request();
        let offer = String::from_utf8_lossy(&initial_request.body).to_string();

        let (ssrc, option) = {
            let call_state = call_state_ref.read().await;
            (
                call_state.ssrc,
                call_state.option.clone().unwrap_or_default(),
            )
        };

        match self.setup_answer_track(ssrc, &option, offer).await {
            Ok((offer, track)) => {
                self.setup_track_with_stream(&option, track).await?;
                {
                    let mut state = self.call_state.write().await;
                    state.ready_to_answer = Some((offer, None, pending_dialog.dialog));
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!("error creating track: {}", e));
            }
        }

        let mut client_dialog_handler =
            DialogStateReceiverGuard::new(self.invitation.dialog_layer.clone(), state_receiver);

        crate::spawn(async move {
            client_dialog_handler.process_dialog(states).await;
        });
        Ok(())
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

impl ActiveCallState {
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
        }
        option
    }

    pub fn set_hangup_reason(&mut self, reason: CallRecordHangupReason) {
        if self.hangup_reason.is_none() {
            self.hangup_reason = Some(reason);
        }
    }

    pub fn build_hangup_event(
        &self,
        track_id: TrackId,
        initiator: Option<String>,
    ) -> crate::event::SessionEvent {
        let from = self.option.as_ref().and_then(|o| o.caller.as_ref());
        let to = self.option.as_ref().and_then(|o| o.callee.as_ref());
        let extra = self.extras.clone();

        crate::event::SessionEvent::Hangup {
            track_id,
            timestamp: crate::media::get_timestamp(),
            reason: Some(format!("{:?}", self.hangup_reason)),
            initiator,
            start_time: self.start_time.to_rfc3339(),
            answer_time: self.answer_time.map(|t| t.to_rfc3339()),
            ringing_time: self.ring_time.map(|t| t.to_rfc3339()),
            hangup_time: Utc::now().to_rfc3339(),
            extra,
            from: from.map(|f| f.into()),
            to: to.map(|f| f.into()),
            refer: Some(self.is_refer),
        }
    }

    pub fn build_callrecord(
        &self,
        app_state: AppState,
        session_id: String,
        call_type: ActiveCallType,
    ) -> CallRecord {
        let option = self.option.clone().unwrap_or_default();
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

        let refer_callrecord = self.refer_callstate.as_ref().and_then(|rc| {
            if let Ok(rc) = rc.try_read() {
                Some(Box::new(rc.build_callrecord(
                    app_state.clone(),
                    rc.session_id.clone(),
                    ActiveCallType::B2bua,
                )))
            } else {
                None
            }
        });

        let caller = option.caller.clone().unwrap_or_default();
        let callee = option.callee.clone().unwrap_or_default();

        CallRecord {
            option: Some(option),
            call_id: session_id,
            call_type,
            start_time: self.start_time,
            ring_time: self.ring_time.clone(),
            answer_time: self.answer_time.clone(),
            end_time: Utc::now(),
            caller,
            callee,
            hangup_reason: self.hangup_reason.clone(),
            hangup_messages: Vec::new(),
            status_code: self.last_status_code,
            extras: self.extras.clone(),
            dump_event_file,
            recorder,
            refer_callrecord,
        }
    }
}
