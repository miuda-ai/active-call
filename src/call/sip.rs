use crate::call::active_call::ActiveCallStateRef;
use crate::callrecord::CallRecordHangupReason;
use crate::event::EventSender;
use crate::media::TrackId;
use crate::media::stream::MediaStream;
use crate::useragent::invitation::PendingDialog;
use anyhow::Result;
use chrono::Utc;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{
    Dialog, DialogState, DialogStateReceiver, DialogStateSender, TerminatedReason,
};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::rsip_ext::RsipResponseExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct DialogStateReceiverGuard {
    pub(super) dialog_layer: Arc<DialogLayer>,
    pub(super) receiver: DialogStateReceiver,
    pub(super) dialog_id: Option<DialogId>,
    pub(super) hangup_headers: Option<Vec<rsip::Header>>,
}

impl DialogStateReceiverGuard {
    pub fn new(
        dialog_layer: Arc<DialogLayer>,
        receiver: DialogStateReceiver,
        hangup_headers: Option<Vec<rsip::Header>>,
    ) -> Self {
        Self {
            dialog_layer,
            receiver,
            dialog_id: None,
            hangup_headers,
        }
    }
    pub async fn recv(&mut self) -> Option<DialogState> {
        let state = self.receiver.recv().await;
        if let Some(ref s) = state {
            self.dialog_id = Some(s.id().clone());
        }
        state
    }

    fn take_dialog(&mut self) -> Option<Dialog> {
        let id = match self.dialog_id.take() {
            Some(id) => id,
            None => return None,
        };

        match self.dialog_layer.get_dialog(&id) {
            Some(dialog) => {
                info!(%id, "dialog removed on  drop");
                self.dialog_layer.remove_dialog(&id);
                return Some(dialog);
            }
            _ => {}
        }
        None
    }

    pub async fn drop_async(&mut self) {
        if let Some(dialog) = self.take_dialog() {
            if let Err(e) = dialog.hangup_with_headers(self.hangup_headers.take()).await {
                warn!(id=%dialog.id(), "error hanging up dialog on drop: {}", e);
            }
        }
    }
}

impl Drop for DialogStateReceiverGuard {
    fn drop(&mut self) {
        if let Some(dialog) = self.take_dialog() {
            crate::spawn(async move {
                if let Err(e) = dialog.hangup().await {
                    warn!(id=%dialog.id(), "error hanging up dialog on drop: {}", e);
                }
            });
        }
    }
}

pub(super) struct InviteDialogStates {
    pub is_client: bool,
    pub session_id: String,
    pub track_id: TrackId,
    pub cancel_token: CancellationToken,
    pub event_sender: EventSender,
    pub call_state: ActiveCallStateRef,
    pub media_stream: Arc<MediaStream>,
    pub terminated_reason: Option<TerminatedReason>,
    pub has_early_media: bool,
}

impl InviteDialogStates {
    pub(super) fn on_terminated(&mut self) {
        let mut call_state_ref = match self.call_state.try_write() {
            Ok(cs) => cs,
            Err(_) => {
                return;
            }
        };
        let reason = &self.terminated_reason;
        call_state_ref.last_status_code = match reason {
            Some(TerminatedReason::UacCancel) => 487,
            Some(TerminatedReason::UacBye) => 200,
            Some(TerminatedReason::UacBusy) => 486,
            Some(TerminatedReason::UasBye) => 200,
            Some(TerminatedReason::UasBusy) => 486,
            Some(TerminatedReason::UasDecline) => 603,
            Some(TerminatedReason::UacOther(code)) => code.code(),
            Some(TerminatedReason::UasOther(code)) => code.code(),
            _ => 500, // Default to internal server error
        };

        if call_state_ref.hangup_reason.is_none() {
            call_state_ref.hangup_reason.replace(match reason {
                Some(TerminatedReason::UacCancel) => CallRecordHangupReason::Canceled,
                Some(TerminatedReason::UacBye) | Some(TerminatedReason::UacBusy) => {
                    CallRecordHangupReason::ByCaller
                }
                Some(TerminatedReason::UasBye) | Some(TerminatedReason::UasBusy) => {
                    CallRecordHangupReason::ByCallee
                }
                Some(TerminatedReason::UasDecline) => CallRecordHangupReason::ByCallee,
                Some(TerminatedReason::UacOther(_)) => CallRecordHangupReason::ByCaller,
                Some(TerminatedReason::UasOther(_)) => CallRecordHangupReason::ByCallee,
                _ => CallRecordHangupReason::BySystem,
            });
        };
        let initiator = match reason {
            Some(TerminatedReason::UacCancel) => "caller".to_string(),
            Some(TerminatedReason::UacBye) | Some(TerminatedReason::UacBusy) => {
                "caller".to_string()
            }
            Some(TerminatedReason::UasBye)
            | Some(TerminatedReason::UasBusy)
            | Some(TerminatedReason::UasDecline) => "callee".to_string(),
            _ => "system".to_string(),
        };
        self.event_sender
            .send(crate::event::SessionEvent::TrackEnd {
                track_id: self.track_id.clone(),
                timestamp: crate::media::get_timestamp(),
                duration: call_state_ref
                    .answer_time
                    .map(|t| (Utc::now() - t).num_milliseconds())
                    .unwrap_or_default() as u64,
                ssrc: call_state_ref.ssrc,
                play_id: None,
            })
            .ok();
        let hangup_event =
            call_state_ref.build_hangup_event(self.track_id.clone(), Some(initiator));
        self.event_sender.send(hangup_event).ok();
    }
}

impl Drop for InviteDialogStates {
    fn drop(&mut self) {
        self.on_terminated();
        self.cancel_token.cancel();
    }
}

impl DialogStateReceiverGuard {
    pub(self) async fn dialog_event_loop(&mut self, states: &mut InviteDialogStates) -> Result<()> {
        while let Some(event) = self.recv().await {
            match event {
                DialogState::Calling(dialog_id) => {
                    info!(session_id=states.session_id, %dialog_id, "dialog calling");
                    states.call_state.write().await.session_id = dialog_id.to_string();
                }
                DialogState::Trying(_) => {}
                DialogState::Early(dialog_id, resp) => {
                    let code = resp.status_code.code();
                    let body = resp.body();
                    let answer = String::from_utf8_lossy(body);
                    let has_sdp = !answer.is_empty();
                    info!(session_id=states.session_id, %dialog_id, has_sdp=%has_sdp, "dialog early ({}): \n{}", code, answer);

                    {
                        let mut cs = states.call_state.write().await;
                        if cs.ring_time.is_none() {
                            cs.ring_time.replace(Utc::now());
                        }
                        cs.last_status_code = code;
                    }

                    if !states.is_client {
                        continue;
                    }

                    let refer = states.call_state.read().await.is_refer;

                    states
                        .event_sender
                        .send(crate::event::SessionEvent::Ringing {
                            track_id: states.track_id.clone(),
                            timestamp: crate::media::get_timestamp(),
                            early_media: has_sdp,
                            refer: Some(refer),
                        })?;

                    if has_sdp {
                        states.has_early_media = true;
                        states
                            .media_stream
                            .update_remote_description(&states.track_id, &answer.to_string())
                            .await?;
                    }
                }
                DialogState::Confirmed(dialog_id, msg) => {
                    info!(session_id=states.session_id, %dialog_id, has_early_media=%states.has_early_media, "dialog confirmed");
                    {
                        let mut cs = states.call_state.write().await;
                        cs.session_id = dialog_id.to_string();
                        cs.answer_time.replace(Utc::now());
                        cs.last_status_code = 200;
                    }
                    if states.is_client {
                        let answer = String::from_utf8_lossy(msg.body());
                        let answer = answer.trim();
                        if !answer.is_empty() {
                            if states.has_early_media {
                                info!(
                                    session_id = states.session_id,
                                    "updating remote description with final answer after early media (force=true)"
                                );
                                // Force update when transitioning from early media (183) to confirmed (200 OK)
                                // This ensures media parameters are properly updated even if SDP appears similar
                                if let Err(e) = states
                                    .media_stream
                                    .update_remote_description_force(
                                        &states.track_id,
                                        &answer.to_string(),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        session_id = states.session_id,
                                        "failed to force update remote description on confirmed: {}",
                                        e
                                    );
                                }
                            } else {
                                if let Err(e) = states
                                    .media_stream
                                    .update_remote_description(
                                        &states.track_id,
                                        &answer.to_string(),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        session_id = states.session_id,
                                        "failed to update remote description on confirmed: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                }
                DialogState::Info(dialog_id, req, tx_handle) => {
                    let body_str = String::from_utf8_lossy(req.body());
                    info!(session_id=states.session_id, %dialog_id, body=%body_str, "dialog info received");
                    if body_str.starts_with("Signal=") {
                        let digit = body_str.trim_start_matches("Signal=").chars().next();
                        if let Some(digit) = digit {
                            states.event_sender.send(crate::event::SessionEvent::Dtmf {
                                track_id: states.track_id.clone(),
                                timestamp: crate::media::get_timestamp(),
                                digit: digit.to_string(),
                            })?;
                        }
                    }
                    tx_handle.reply(rsip::StatusCode::OK).await.ok();
                }
                DialogState::Updated(dialog_id, _req, tx_handle) => {
                    info!(session_id = states.session_id, %dialog_id, "dialog update received");
                    let mut answer_sdp = None;
                    if let Some(sdp_body) = _req.body().get(..) {
                        let sdp_str = String::from_utf8_lossy(sdp_body);
                        if !sdp_str.is_empty()
                            && (_req.method == rsip::Method::Invite
                                || _req.method == rsip::Method::Update)
                        {
                            info!(session_id=states.session_id, %dialog_id, method=%_req.method, "handling re-invite/update offer");
                            match states
                                .media_stream
                                .handshake(&states.track_id, sdp_str.to_string(), None)
                                .await
                            {
                                Ok(sdp) => answer_sdp = Some(sdp),
                                Err(e) => {
                                    warn!(
                                        session_id = states.session_id,
                                        "failed to handle re-invite: {}", e
                                    );
                                }
                            }
                        } else {
                            info!(session_id=states.session_id, %dialog_id, "updating remote description:\n{}", sdp_str);
                            states
                                .media_stream
                                .update_remote_description(&states.track_id, &sdp_str.to_string())
                                .await?;
                        }
                    }

                    if let Some(sdp) = answer_sdp {
                        tx_handle
                            .respond(
                                rsip::StatusCode::OK,
                                Some(vec![rsip::Header::ContentType(
                                    "application/sdp".to_string().into(),
                                )]),
                                Some(sdp.into()),
                            )
                            .await
                            .ok();
                    } else {
                        tx_handle.reply(rsip::StatusCode::OK).await.ok();
                    }
                }
                DialogState::Options(dialog_id, _req, tx_handle) => {
                    info!(session_id = states.session_id, %dialog_id, "dialog options received");
                    tx_handle.reply(rsip::StatusCode::OK).await.ok();
                }
                DialogState::Terminated(dialog_id, reason) => {
                    info!(
                        session_id = states.session_id,
                        ?dialog_id,
                        ?reason,
                        "dialog terminated"
                    );
                    states.terminated_reason = Some(reason.clone());
                    return Ok(());
                }
                other_state => {
                    info!(
                        session_id = states.session_id,
                        %other_state,
                        "dialog received other state"
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) async fn process_dialog(&mut self, mut states: InviteDialogStates) {
        let token = states.cancel_token.clone();
        tokio::select! {
            _ = token.cancelled() => {
                states.terminated_reason = Some(TerminatedReason::UacCancel);
            }
            _ = self.dialog_event_loop(&mut states) => {}
        };

        // Update hangup headers from ActiveCallState if available
        {
            let state = states.call_state.read().await;
            if let Some(extras) = &state.extras {
                if let Some(h_val) = extras.get("_sip_headers") {
                    if let Ok(headers_map) =
                        serde_json::from_value::<HashMap<String, String>>(h_val.clone())
                    {
                        let mut headers = Vec::new();
                        for (k, v) in headers_map {
                            headers.push(rsip::Header::Other(k.into(), v.into()));
                        }
                        if !headers.is_empty() {
                            if let Some(existing) = &mut self.hangup_headers {
                                existing.extend(headers);
                            } else {
                                self.hangup_headers = Some(headers);
                            }
                        }
                    }
                }
            }
        }

        self.drop_async().await;
    }
}

#[derive(Clone)]
pub struct Invitation {
    pub dialog_layer: Arc<DialogLayer>,
    pub pending_dialogs: Arc<std::sync::Mutex<HashMap<DialogId, PendingDialog>>>,
}

impl Invitation {
    pub fn new(dialog_layer: Arc<DialogLayer>) -> Self {
        Self {
            dialog_layer,
            pending_dialogs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn add_pending(&self, dialog_id: DialogId, pending: PendingDialog) {
        self.pending_dialogs
            .lock()
            .map(|mut ps| ps.insert(dialog_id, pending))
            .ok();
    }

    pub fn get_pending_call(&self, dialog_id: &DialogId) -> Option<PendingDialog> {
        self.pending_dialogs
            .lock()
            .ok()
            .and_then(|mut ps| ps.remove(dialog_id))
    }

    pub fn has_pending_call(&self, dialog_id: &DialogId) -> bool {
        self.pending_dialogs
            .lock()
            .ok()
            .map(|ps| ps.contains_key(dialog_id))
            .unwrap_or(false)
    }

    pub fn find_dialog_id_by_session_id(&self, session_id: &str) -> Option<DialogId> {
        self.pending_dialogs.lock().ok().and_then(|ps| {
            ps.iter()
                .find(|(id, _)| id.to_string() == session_id)
                .map(|(id, _)| id.clone())
        })
    }

    pub async fn hangup(
        &self,
        dialog_id: DialogId,
        code: Option<rsip::StatusCode>,
        reason: Option<String>,
    ) -> Result<()> {
        if let Some(call) = self.get_pending_call(&dialog_id) {
            call.dialog.reject(code, reason).ok();
            call.token.cancel();
        }
        match self.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => {
                self.dialog_layer.remove_dialog(&dialog_id);
                dialog.hangup().await.ok();
            }
            None => {}
        }
        Ok(())
    }

    pub async fn reject(&self, dialog_id: DialogId) -> Result<()> {
        if let Some(call) = self.get_pending_call(&dialog_id) {
            call.dialog.reject(None, None).ok();
            call.token.cancel();
        }
        match self.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => {
                self.dialog_layer.remove_dialog(&dialog_id);
                dialog.hangup().await.ok();
            }
            None => {}
        }
        Ok(())
    }

    pub async fn invite(
        &self,
        invite_option: InviteOption,
        state_sender: DialogStateSender,
    ) -> Result<(DialogId, Option<Vec<u8>>), rsipstack::Error> {
        let (dialog, resp) = self
            .dialog_layer
            .do_invite(invite_option, state_sender)
            .await?;

        let offer = match resp {
            Some(resp) => match resp.status_code.kind() {
                rsip::StatusCodeKind::Successful => {
                    let offer = resp.body.clone();
                    Some(offer)
                }
                _ => {
                    let reason = resp
                        .reason_phrase()
                        .unwrap_or(&resp.status_code.to_string())
                        .to_string();
                    return Err(rsipstack::Error::DialogError(
                        reason,
                        dialog.id(),
                        resp.status_code,
                    ));
                }
            },
            None => {
                return Err(rsipstack::Error::DialogError(
                    "no response received".to_string(),
                    dialog.id(),
                    rsip::StatusCode::NotAcceptableHere,
                ));
            }
        };
        Ok((dialog.id(), offer))
    }
}
