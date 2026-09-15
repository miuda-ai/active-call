//! Track construction and wiring for [`ActiveCall`].
//!
//! Split out of `active_call.rs` so the call-flow orchestration stays separate
//! from the (heavily reused) track assembly building blocks.

use super::active_call::{ActiveCall, ActiveCallType, PendingCallerTrack};
use super::state::LegShared;
use crate::CallOption;
use crate::event::SessionEvent;
use crate::media::TrackId;
use crate::media::ambiance::SharedAmbianceProcessor;
use crate::media::engine::StreamEngine;
use crate::media::negotiate::strip_ipv6_candidates;
use crate::media::processor::SubscribeProcessor;
use crate::media::track::Track;
use crate::media::track::file::FileTrack;
use crate::media::track::rtc::{RtcTrack, RtcTrackConfig};
use crate::media::track::websocket::{WebsocketBytesReceiver, WebsocketTrack};
use crate::useragent::invitation::PendingDialog;
use crate::useragent::public_address::{
    build_public_contact_uri, contact_needs_public_resolution, find_local_addr_for_uri,
};
use anyhow::Result;
use audio_codec::CodecType;
use chrono::Utc;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::rsip::prelude::HeadersExt;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::active_call::PendingSipAnswer;
use super::sip::{DialogStateReceiverGuard, InviteDialogStates};

/// Track id of the SIP leg of a non-SIP call type (WebSocket/Webrtc) that was
/// created by an inbound SIP INVITE. Must stay distinct from the caller track
/// (`session_id`) and the refer leg (`server_side_track_id`) so the three
/// legs coexist in the media stream's full-mesh forwarding.
const SIP_LEG_TRACK_ID: &str = "sip-leg-track";

/// Restrict a track's codec capabilities to codecs the remote offer actually
/// advertises. rustrtc's `create_answer` otherwise emits its default
/// capability set (e.g. opus-only), which intersects to an *empty* codec list
/// against a plain PCMU/PCMA SIP offer (`intersect_answer` then produces an
/// unparseable `m=` line). Configured codecs win when they overlap the offer;
/// the offer wins when they don't.
fn restrict_codecs_to_offer(rtc_config: &mut RtcTrackConfig, offer: &str) {
    let offer_codecs: Vec<CodecType> = offer
        .lines()
        .filter_map(|line| {
            let value = line.trim().strip_prefix("a=rtpmap:")?;
            crate::media::negotiate::parse_rtpmap(value)
                .ok()
                .map(|(_, codec, ..)| codec)
        })
        .collect();
    if offer_codecs.is_empty() {
        return;
    }
    if rtc_config.codecs.is_empty() {
        rtc_config.codecs = offer_codecs;
    } else {
        rtc_config.codecs.retain(|c| offer_codecs.contains(c));
        if rtc_config.codecs.is_empty() {
            rtc_config.codecs = offer_codecs;
        }
    }
}

/// Everything needed to place one outgoing INVITE (main leg or refer leg).
pub(super) struct OutgoingLeg {
    /// Lifetime token of the leg (call token or the refer-specific child).
    pub cancel_token: CancellationToken,
    /// Lock-free shared state of the leg being established.
    pub leg: LegShared,
    /// Media track id for the leg.
    pub track_id: TrackId,
    /// The outgoing INVITE (offer may be filled in by the caller).
    pub invite_option: InviteOption,
    /// Call option used to build media processors / MOH.
    pub call_option: CallOption,
    /// Music-on-hold path to play while the INVITE is in flight.
    pub moh: Option<String>,
    /// Refer legs with `auto_hangup`: hang up the parent call when this leg ends.
    pub auto_hangup: bool,
}

impl ActiveCall {
    /// Apply the configured codec list to an RTC track config.
    fn rtc_apply_codecs(&self, rtc_config: &mut RtcTrackConfig) {
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
    }

    /// Apply external/bind addresses from the global config.
    fn rtc_apply_network(&self, rtc_config: &mut RtcTrackConfig) {
        if let Some(ref external_ip) = self.app_state.config.external_ip {
            rtc_config.external_ip = Some(external_ip.clone());
        }
        if let Some(ref bind_ip) = self.app_state.config.rtp_bind_ip {
            rtc_config.bind_ip = Some(bind_ip.clone());
        }
    }

    /// Apply RTP latching and ICE-lite flags (per-call option overrides config).
    fn rtc_apply_latching(&self, rtc_config: &mut RtcTrackConfig) {
        rtc_config.enable_latching = self.app_state.config.enable_rtp_latching;
        rtc_config.enable_ice_lite = self
            .progress
            .load()
            .option
            .as_ref()
            .and_then(|o| o.enable_ice_lite)
            .or(self.app_state.config.enable_ice_lite);
    }

    /// Build a looping file track on the server-side track.
    pub(super) fn make_file_track(&self, path: String, ssrc: u32) -> FileTrack {
        FileTrack::new(self.server_side_track_id.clone())
            .with_play_id(Some(path.clone()))
            .with_ssrc(ssrc)
            .with_path(path)
            .with_cancel_token(self.cancel_token.child_token())
    }

    /// Emit a `Reject` session event from an rsipstack dialog error.
    pub(super) fn emit_reject_from_rsip_error(
        &self,
        track_id: TrackId,
        refer: bool,
        e: &rsipstack::Error,
    ) {
        if let rsipstack::Error::DialogError(reason, _, code) = e {
            self.event_sender
                .send(SessionEvent::Reject {
                    track_id,
                    timestamp: crate::media::get_timestamp(),
                    reason: reason.clone(),
                    code: Some(code.code() as u32),
                    refer: Some(refer),
                })
                .ok();
        }
    }

    /// If a pending incoming dialog exists for this session, start preparing
    /// the incoming SIP track; shared by the Sip and B2bua call types.
    pub(super) async fn try_prepare_incoming_sip_track(
        &self,
        hangup_headers: Option<Vec<rsipstack::rsip::Header>>,
    ) -> Option<Result<()>> {
        let dialog_id = self
            .invitation
            .find_dialog_id_by_session_id(&self.session_id)?;
        let pending_dialog = self.invitation.get_pending_call(&dialog_id)?;
        Some(
            self.prepare_incoming_sip_track(pending_dialog, hangup_headers)
                .await,
        )
    }

    /// For non-SIP call types (WebSocket/Webrtc) whose session was created by
    /// an inbound SIP INVITE, build the SIP leg's answering track and remember
    /// the prepared 200 OK. `do_accept` sends the answer and registers the
    /// track, so the carrier leg is actually established instead of sitting in
    /// `Trying` until the far end times out (production: VOS3000 CANCELs the
    /// unanswered INVITE after 20s). No-op when there is no pending dialog or
    /// the INVITE carries no offer.
    pub(super) async fn try_prepare_pending_sip_answer(
        &self,
        option: &CallOption,
    ) -> Result<()> {
        let Some(dialog_id) = self
            .invitation
            .find_dialog_id_by_session_id(&self.session_id)
        else {
            return Ok(());
        };
        let Some(pending_dialog) = self.invitation.get_pending_call(&dialog_id) else {
            return Ok(());
        };

        let initial_request = pending_dialog.dialog.initial_request();
        let offer = String::from_utf8_lossy(initial_request.body()).to_string();
        debug!(
            session_id = self.session_id,
            offer = %offer,
            "preparing sip leg answer"
        );
        if offer.trim().is_empty() {
            warn!(
                session_id = self.session_id,
                "inbound SIP dialog has no SDP offer; skipping sip leg preparation"
            );
            return Ok(());
        }

        // The SIP leg needs its own track id and SSRC: `session_id` is the
        // caller (WebSocket/Webrtc) track and `server_side_track_id` belongs
        // to the refer leg.
        let track_id: TrackId = SIP_LEG_TRACK_ID.to_string();
        let ssrc = rand::random::<u32>();

        let mut rtc_config = RtcTrackConfig::default();
        let use_srtp = option
            .sip
            .as_ref()
            .and_then(|s| s.enable_srtp)
            .or(self.app_state.config.enable_srtp)
            .unwrap_or(false);
        rtc_config.mode = if use_srtp {
            rustrtc::TransportMode::Srtp
        } else {
            rustrtc::TransportMode::Rtp
        };
        self.rtc_apply_codecs(&mut rtc_config);
        if rtc_config.preferred_codec.is_none() {
            rtc_config.preferred_codec = Some(self.track_config.codec);
        }
        rtc_config.rtp_port_range = self
            .app_state
            .config
            .rtp_start_port
            .zip(self.app_state.config.rtp_end_port);
        self.rtc_apply_network(&mut rtc_config);
        self.rtc_apply_latching(&mut rtc_config);
        restrict_codecs_to_offer(&mut rtc_config, &offer);

        let mut sip_track = RtcTrack::new(
            self.cancel_token.child_token(),
            track_id.clone(),
            self.track_config.clone(),
            rtc_config,
        )
        .with_ssrc(ssrc);
        sip_track.create().await?;

        let timeout = option.handshake_timeout.map(|t| Duration::from_secs(t));
        let answer = sip_track
            .handshake(offer, timeout)
            .await
            .map_err(|e| anyhow::anyhow!("sip leg handshake failed: {e}"))?;

        // Drive the incoming dialog's state machine: hangs the dialog up when
        // the call ends, and ends cleanly on a far-end BYE/CANCEL.
        let states = InviteDialogStates::new(
            false,
            self.session_id.clone(),
            track_id.clone(),
            self.event_sender.clone(),
            self.media_stream.clone(),
            self.leg(),
            self.cancel_token.clone(),
            None,
        );
        let hangup_headers = option
            .sip
            .as_ref()
            .and_then(|s| s.hangup_headers.as_ref())
            .map(crate::sip_util::sip_headers_from_map);
        let mut client_dialog_handler = DialogStateReceiverGuard::new(
            self.invitation.dialog_layer.clone(),
            pending_dialog.state_receiver,
            hangup_headers,
        );
        crate::spawn(async move {
            client_dialog_handler.process_dialog(states).await;
        });

        info!(
            session_id = self.session_id,
            track_id, "prepared sip leg answer for non-sip call type"
        );

        self.set_pending_sip_answer(PendingSipAnswer {
            answer,
            dialog: pending_dialog.dialog,
            track: Box::new(sip_track),
        });
        Ok(())
    }

    /// Per-call ambiance option merged over the global config.
    fn merged_ambiance(
        &self,
        call_ambiance: Option<&crate::media::ambiance::AmbianceOption>,
    ) -> crate::media::ambiance::AmbianceOption {
        let mut opt = call_ambiance.cloned().unwrap_or_default();
        if let Some(global) = &self.app_state.config.ambiance {
            opt.merge(global);
        }
        opt
    }

    pub(super) async fn create_rtp_track(
        &self,
        track_id: TrackId,
        ssrc: u32,
        enable_srtp: Option<bool>,
        offer: Option<&str>,
    ) -> Result<RtcTrack> {
        let mut rtc_config = RtcTrackConfig::default();
        // Per-call flag takes precedence over global config.
        let use_srtp = enable_srtp
            .or(self.app_state.config.enable_srtp)
            .unwrap_or(false);
        rtc_config.mode = if use_srtp {
            rustrtc::TransportMode::Srtp
        } else {
            rustrtc::TransportMode::Rtp
        };

        self.rtc_apply_codecs(&mut rtc_config);
        if let Some(offer) = offer {
            restrict_codecs_to_offer(&mut rtc_config, offer);
        }

        if rtc_config.preferred_codec.is_none() {
            rtc_config.preferred_codec = Some(self.track_config.codec.clone());
        }

        rtc_config.rtp_port_range = self
            .app_state
            .config
            .rtp_start_port
            .zip(self.app_state.config.rtp_end_port);

        self.rtc_apply_network(&mut rtc_config);
        self.rtc_apply_latching(&mut rtc_config);

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

    pub(super) async fn setup_track_with_stream(
        &self,
        option: &CallOption,
        mut track: Box<dyn Track>,
    ) -> Result<()> {
        let processors = match StreamEngine::create_processors(
            self.app_state.stream_engine.clone(),
            track.id().clone(),
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

    pub(super) async fn update_track_wrapper(
        &self,
        mut track: Box<dyn Track>,
        play_id: Option<String>,
    ) {
        let (call_ambiance, subscribe) = {
            let state = self.progress.load_full();
            (
                state.option.as_ref().and_then(|o| o.ambiance.clone()),
                state
                    .option
                    .as_ref()
                    .and_then(|o| o.subscribe)
                    .unwrap_or_default(),
            )
        };
        let ambiance_opt = self.merged_ambiance(call_ambiance.as_ref());

        let shared_ambiance = match self
            .media_stream
            .ensure_ambiance(ambiance_opt, self.server_side_track_id.clone())
            .await
        {
            Ok(shared) => shared,
            Err(e) => {
                tracing::error!("failed to load ambiance wav {}", e);
                None
            }
        };

        if track.id() == &self.server_side_track_id {
            if let Some(shared) = shared_ambiance {
                info!(session_id = self.session_id, "loaded ambiance processor");
                track.append_processor(Box::new(SharedAmbianceProcessor::new(shared)));
            }
        }

        if subscribe && self.call_type != ActiveCallType::WebSocket {
            let (track_index, sub_track_id) = if track.id() == &self.server_side_track_id {
                (0, self.server_side_track_id.clone())
            } else {
                (1, self.session_id.clone())
            };
            let sub_processor =
                SubscribeProcessor::new(self.event_sender.clone(), sub_track_id, track_index);
            track.append_processor(Box::new(sub_processor));
        }

        self.set_current_play(play_id.clone());
        self.media_stream.update_track(track, play_id).await;
    }

    pub(super) async fn setup_caller_track(&self, option: &CallOption) -> Result<()> {
        let hangup_headers = option
            .sip
            .as_ref()
            .and_then(|s| s.hangup_headers.as_ref())
            .map(crate::sip_util::sip_headers_from_map);
        self.set_option(option.clone());
        info!(
            session_id = self.session_id,
            call_type = ?self.call_type,
            "setup caller track"
        );

        let track = match self.call_type {
            ActiveCallType::Webrtc => {
                let track = self.create_webrtc_track().await?;
                // The session may be backed by a ringing inbound SIP dialog;
                // prepare its answer so do_accept establishes the carrier leg.
                self.try_prepare_pending_sip_answer(option).await?;
                Some(track)
            }
            ActiveCallType::WebSocket => {
                let audio_receiver = self.audio_receiver.lock().unwrap().take();
                if let Some(receiver) = audio_receiver {
                    let track = self.create_websocket_track(receiver).await?;
                    // The session may be backed by a ringing inbound SIP
                    // dialog; prepare its answer so do_accept establishes the
                    // carrier leg (otherwise it stays in Trying until the far
                    // end CANCELs).
                    self.try_prepare_pending_sip_answer(option).await?;
                    Some(track)
                } else {
                    None
                }
            }
            ActiveCallType::Sip | ActiveCallType::B2bua => {
                // Incoming call with a pending dialog: prepare the answering leg.
                if let Some(result) = self.try_prepare_incoming_sip_track(hangup_headers).await {
                    return result;
                }

                if matches!(self.call_type, ActiveCallType::B2bua) {
                    warn!(
                        session_id = self.session_id,
                        "no pending dialog found for B2BUA call"
                    );
                    return Err(anyhow::anyhow!(
                        "no pending dialog found for session_id: {}",
                        self.session_id
                    ));
                }

                // Outgoing SIP call below (Sip only).
                // Auto-inject credentials from registered users if not already provided
                let mut option = option.clone();
                if option.sip.is_none()
                    || option
                        .sip
                        .as_ref()
                        .and_then(|s| s.username.as_ref())
                        .is_none()
                {
                    if let Some(callee) = &option.callee {
                        if let Some(cred) = self.app_state.find_credentials_for_callee(callee) {
                            if option.sip.is_none() {
                                option.sip = Some(crate::SipOption {
                                    username: Some(cred.username.clone()),
                                    password: Some(cred.password.clone()),
                                    realm: cred.realm.clone(),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }

                let mut invite_option = option.build_invite_option()?;
                invite_option.call_id = Some(self.session_id.clone());

                let out = OutgoingLeg {
                    cancel_token: self.cancel_token.clone(),
                    leg: self.leg(),
                    track_id: self.session_id.clone(),
                    invite_option,
                    call_option: option.clone(),
                    moh: None,
                    auto_hangup: false,
                };
                match self.create_outgoing_sip_track(out).await {
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
                        self.emit_reject_from_rsip_error(self.session_id.clone(), false, &e);
                        return Err(e.into());
                    }
                }
            }
        };
        match track {
            Some(track) => {
                self.finish_caller_stack(&option, PendingCallerTrack::NotStarted(track))
                    .await?;
            }
            None => {
                warn!(session_id = self.session_id, "no track created for caller");
                return Err(anyhow::anyhow!("no track created for caller"));
            }
        }
        Ok(())
    }

    pub(super) async fn finish_caller_stack(
        &self,
        option: &CallOption,
        pending_track: PendingCallerTrack,
    ) -> Result<()> {
        self.ensure_call_ambiance(option).await;
        match pending_track {
            PendingCallerTrack::NotStarted(track) => {
                self.setup_track_with_stream(option, track).await?;
            }
            PendingCallerTrack::StartedForEarlyMedia => {
                // Track is already running in the media stream (started during ringing
                // for early-media ringtone). Build processors from the accept option
                // and append them to the running caller track.
                let track_id = self.session_id.clone();
                let processors = StreamEngine::create_processors(
                    self.app_state.stream_engine.clone(),
                    track_id.clone(),
                    self.cancel_token.child_token(),
                    self.event_sender.clone(),
                    self.media_stream.packet_sender.clone(),
                    option,
                )
                .await
                .unwrap_or_else(|e| {
                    warn!(
                        session_id = self.session_id,
                        "failed to create processors on accept: {}", e
                    );
                    vec![]
                });
                for processor in processors {
                    self.media_stream
                        .append_processor(&track_id, processor)
                        .await
                        .ok();
                }
            }
        }

        {
            let call_state = self.progress.load_full();
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

    pub(super) async fn ensure_call_ambiance(&self, option: &CallOption) {
        let opt = self.merged_ambiance(option.ambiance.as_ref());
        if let Err(e) = self
            .media_stream
            .ensure_ambiance(opt, self.server_side_track_id.clone())
            .await
        {
            tracing::error!(
                session_id = self.session_id,
                "failed to load ambiance wav {}",
                e
            );
        }
    }

    pub(super) async fn create_websocket_track(
        &self,
        audio_receiver: WebsocketBytesReceiver,
    ) -> Result<Box<dyn Track>> {
        let codec = self
            .progress
            .load_full()
            .option
            .as_ref()
            .map(|o| o.codec.clone())
            .unwrap_or_default();

        let ws_track = WebsocketTrack::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            self.track_config.clone(),
            self.event_sender.clone(),
            audio_receiver,
            codec,
            self.ssrc,
        );

        self.leg().update_progress(|p| {
            p.answer = Some("".to_string());
            p.on_answered();
        });

        Ok(Box::new(ws_track))
    }

    pub(super) async fn create_webrtc_track(&self) -> Result<Box<dyn Track>> {
        let option = self.progress.load_full().option.clone().unwrap_or_default();
        let ssrc = self.ssrc;

        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = rustrtc::TransportMode::WebRtc; // WebRTC
        rtc_config.ice_servers = self.app_state.config.ice_servers.clone();

        self.rtc_apply_codecs(&mut rtc_config);
        self.rtc_apply_network(&mut rtc_config);

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

        self.leg().update_progress(|p| {
            p.answer = answer.clone();
            p.on_answered();
        });
        Ok(Box::new(webrtc_track))
    }

    pub(super) async fn create_outgoing_sip_track(
        &self,
        mut out: OutgoingLeg,
    ) -> Result<String, rsipstack::Error> {
        // Apply trunk rules (match + rewrite caller/callee/contact) to the
        // outgoing INVITE/REFER before it is sent. Covers both normal invite
        // calls and refer legs since both flow through this function.
        self.app_state
            .config
            .apply_trunk_rules(&mut out.invite_option);

        let track_id = &out.track_id;
        let ssrc = out.leg.ssrc;
        let per_call_srtp = out.call_option.sip.as_ref().and_then(|s| s.enable_srtp);
        let rtp_track = self
            .create_rtp_track(track_id.clone(), ssrc, per_call_srtp, None)
            .await
            .map_err(|e| rsipstack::Error::Error(e.to_string()))?;

        let offer = Some(
            rtp_track
                .local_description()
                .await
                .map_err(|e| rsipstack::Error::Error(e.to_string()))?,
        );

        out.leg.update_progress(|p| {
            if let Some(o) = p.option.as_mut() {
                o.offer = offer.clone();
            }
            p.start_time = Some(Utc::now());
        });

        let invite_option = &mut out.invite_option;
        invite_option.offer = offer.clone().map(|s| s.into());

        // Set contact to local SIP endpoint address if not already set explicitly
        // Check if contact is still default (no scheme set) or if host is localhost-like
        let needs_contact = contact_needs_public_resolution(&invite_option.contact);

        if needs_contact {
            let addrs = self.invitation.dialog_layer.endpoint.get_addrs();
            if let Some(addr) = find_local_addr_for_uri(&addrs, &invite_option.callee) {
                let contact_username = invite_option
                    .contact
                    .auth
                    .as_ref()
                    .map(|auth| auth.user.as_str())
                    .or_else(|| {
                        invite_option
                            .caller
                            .auth
                            .as_ref()
                            .map(|auth| auth.user.as_str())
                    });
                invite_option.contact = build_public_contact_uri(
                    &self.app_state.learned_public_address,
                    self.app_state.auto_learn_public_address_enabled(),
                    &addr,
                    contact_username,
                    Some(&invite_option.contact),
                );
            } else {
                return Err(rsipstack::Error::Error(format!(
                    "missing local SIP address for callee transport: {}",
                    invite_option.callee
                )));
            }
        }

        let mut rtp_track_to_setup = Some(Box::new(rtp_track) as Box<dyn Track>);

        if let Some(moh) = out.moh.take() {
            let ssrc_and_moh = {
                self.set_moh(Some(moh.clone()));
                if self.current_play().is_none() {
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
                let file_track = self.make_file_track(moh_path.clone(), ssrc);
                self.update_track_wrapper(Box::new(file_track), Some(moh_path))
                    .await;
            }
        } else {
            let track = rtp_track_to_setup.take().unwrap();
            self.setup_track_with_stream(&out.call_option, track)
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

        let states = InviteDialogStates::new(
            true,
            self.session_id.clone(),
            track_id.clone(),
            self.event_sender.clone(),
            self.media_stream.clone(),
            out.leg.clone(),
            out.cancel_token.clone(),
            out.auto_hangup.then_some(crate::callrecord::CallRecordHangupReason::ByRefer),
        );

        let hangup_headers = out
            .call_option
            .sip
            .as_ref()
            .and_then(|s| s.hangup_headers.as_ref())
            .map(crate::sip_util::sip_headers_from_map);

        let mut client_dialog_handler = DialogStateReceiverGuard::new(
            self.invitation.dialog_layer.clone(),
            dlg_state_receiver,
            hangup_headers,
        );

        crate::spawn(async move {
            client_dialog_handler.process_dialog(states).await;
        });

        let (dialog_id, answer) = self
            .invitation
            .invite(out.invite_option, dlg_state_sender)
            .await?;

        if out.cancel_token.is_cancelled() {
            // CANCEL and the far end's 2xx crossed in flight (RFC 3261 S9.1
            // glare) - e.g. a slow PSTN gateway had already committed to
            // alerting the handset by the time our CANCEL arrived, and it
            // answered anyway. do_invite already ACKed the 2xx; per spec we
            // must now BYE it instead of treating this as a normal answer -
            // the ActiveCall session this invite belongs to is already gone.
            if let Some(dialog) = self.invitation.dialog_layer.get_dialog(&dialog_id) {
                if let Err(e) = dialog.hangup().await {
                    warn!(
                        session_id = self.session_id,
                        "failed to BYE a late-confirmed cancelled invite: {}", e
                    );
                }
            }
            return Err(rsipstack::Error::DialogError(
                "invite was cancelled before this late answer arrived".to_string(),
                dialog_id,
                rsipstack::rsip::StatusCode::RequestTerminated,
            ));
        }

        self.set_moh(None);

        if let Some(track) = rtp_track_to_setup {
            info!(
                session_id = self.session_id,
                track_id, "Stopping MOH and setting up RTP track"
            );
            self.media_stream
                .remove_track(&self.server_side_track_id, false)
                .await;

            self.setup_track_with_stream(&out.call_option, track)
                .await
                .map_err(|e| rsipstack::Error::Error(e.to_string()))?;
        }

        // Resolve the final answer SDP, falling back to the early-media (183)
        // SDP when the 200 OK carries no body.
        let early_answer = out.leg.progress.load_full().answer.clone();
        let (answer, remote_description_already_applied) =
            match crate::call::state::resolve_final_answer(answer, early_answer.as_ref()) {
                Ok(resolved) => resolved,
                Err(msg) => {
                    warn!(session_id = self.session_id, "{}", msg);
                    return Err(rsipstack::Error::DialogError(
                        "No answer received".to_string(),
                        dialog_id,
                        rsipstack::rsip::StatusCode::NotAcceptableHere,
                    ));
                }
            };

        out.leg.update_progress(|p| p.try_set_answer(&answer));

        if remote_description_already_applied {
            // The 200 OK carried no body, so the early-media (183) SDP *is*
            // the answer. It was applied provisionally (Pranswer) while
            // ringing; re-apply it as a final Answer so the peer connection's
            // signaling state stabilizes. A plain update would be skipped by
            // the SDP-equality fast path, so force it.
            self.media_stream
                .update_remote_description_force(&track_id, &answer)
                .await
                .ok();
        } else {
            self.media_stream
                .update_remote_description(&track_id, &answer)
                .await
                .ok();
        }

        Ok(answer)
    }

    /// Detect if SDP is WebRTC format
    pub(super) fn is_webrtc_sdp(sdp: &str) -> bool {
        (sdp.contains("a=ice-ufrag:") || sdp.contains("a=ice-pwd:"))
            && sdp.contains("a=fingerprint:")
    }

    pub(super) async fn setup_answer_track(
        &self,
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
            self.rtc_apply_network(&mut rtc_config);
            self.rtc_apply_latching(&mut rtc_config);
            restrict_codecs_to_offer(&mut rtc_config, &offer);

            let webrtc_track = RtcTrack::new(
                self.cancel_token.child_token(),
                self.session_id.clone(),
                self.track_config.clone(),
                rtc_config,
            )
            .with_ssrc(self.ssrc);

            Box::new(webrtc_track) as Box<dyn Track>
        } else {
            let per_call_srtp = option.sip.as_ref().and_then(|s| s.enable_srtp);
            let rtp_track = self
                .create_rtp_track(
                    self.session_id.clone(),
                    self.ssrc,
                    per_call_srtp,
                    Some(&offer),
                )
                .await?;
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

    pub(super) async fn prepare_incoming_sip_track(
        &self,
        pending_dialog: PendingDialog,
        hangup_headers: Option<Vec<rsipstack::rsip::Header>>,
    ) -> Result<()> {
        let state_receiver = pending_dialog.state_receiver;

        let states = InviteDialogStates::new(
            false,
            self.session_id.clone(),
            self.session_id.clone(),
            self.event_sender.clone(),
            self.media_stream.clone(),
            self.leg(),
            self.cancel_token.clone(),
            None,
        );

        let initial_request = pending_dialog.dialog.initial_request();
        let offer = String::from_utf8_lossy(&initial_request.body).to_string();

        let caller = initial_request
            .from_header()
            .ok()
            .and_then(|h| h.uri().ok())
            .map(|u| u.to_string())
            .unwrap_or_default();
        let callee = initial_request
            .to_header()
            .ok()
            .and_then(|h| h.uri().ok())
            .map(|u| u.to_string())
            .unwrap_or_default();
        let headers: Option<std::collections::HashMap<String, String>> = {
            let mut h = std::collections::HashMap::new();
            for header in initial_request.headers.iter() {
                if let rsipstack::rsip::Header::Other(name, value) = header {
                    h.insert(name.to_string(), value.to_string());
                }
            }
            if h.is_empty() { None } else { Some(h) }
        };
        self.event_sender
            .send(SessionEvent::Incoming {
                track_id: self.session_id.clone(),
                timestamp: crate::media::get_timestamp(),
                caller,
                callee,
                sdp: offer.clone(),
                headers,
            })
            .ok();

        let option = self.progress.load_full().option.clone().unwrap_or_default();

        match self.setup_answer_track(&option, offer).await {
            Ok((offer, track)) => {
                // Start the track in the media stream now — early-media ringtone
                // requires the RTP sender loop to be running during ringing.
                // Processors are intentionally omitted here; they will be built from
                // the accept option (which carries VAD/ASR/AGC config) when Accept
                // is issued, via finish_caller_stack(StartedForEarlyMedia).
                //
                // Do NOT call setup_track_with_stream here: it builds the VAD/ASR/AGC
                // processors from the stored option. When Accept arrives without a prior
                // Ringing, that option already carries `asr`, so processors would be built
                // both here and again in finish_caller_stack(StartedForEarlyMedia),
                // resulting in two ASR clients (two WebSocket connections) per session.
                // update_track_wrapper only starts the track (plus ambiance/subscribe),
                // deferring all VAD/ASR/AGC processors to accept time.
                self.update_track_wrapper(track, None).await;
                self.set_ready_to_answer(crate::call::active_call::ReadyAnswer {
                    answer: offer,
                    track: PendingCallerTrack::StartedForEarlyMedia,
                    dialog: pending_dialog.dialog,
                });
            }
            Err(e) => {
                return Err(anyhow::anyhow!("error creating track: {}", e));
            }
        }

        let mut client_dialog_handler = DialogStateReceiverGuard::new(
            self.invitation.dialog_layer.clone(),
            state_receiver,
            hangup_headers,
        );

        crate::spawn(async move {
            client_dialog_handler.process_dialog(states).await;
        });
        Ok(())
    }
}
