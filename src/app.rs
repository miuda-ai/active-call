use crate::{
    call::{ActiveCallRef, refer_headers::ReferHeaderResolver, sip::Invitation},
    callrecord::{
        CallRecordFormatter, CallRecordManagerBuilder, CallRecordSender, DefaultCallRecordFormatter,
    },
    config::Config,
    locator::RewriteTargetLocator,
    sipflow::{SipFlow, SipFlowUploadHook},
    useragent::{
        RegisterOption,
        invitation::{
            FnCreateInvitationHandler, PendingDialog, PendingDialogGuard,
            default_create_invite_handler,
        },
        peer_learning::{PeerAddressLearner, SharedLearnedPeers},
        public_address::{
            LearningMessageInspector, SharedPublicAddress, build_contact, build_public_contact_uri,
            find_local_addr_for_uri,
        },
        registration::{RegistrationHandle, UserCredential},
    },
};

use crate::media::{cache::set_cache_dir, engine::StreamEngine};
use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use futures::FutureExt;
use humantime::parse_duration;
use rsipstack::rsip::prelude::HeadersExt;
use rsipstack::rsip::{Accept, Method, typed};
use rsipstack::transaction::{
    Endpoint, TransactionReceiver,
    endpoint::{TargetLocator, TransportEventInspector},
};
use rsipstack::transport::transport_layer::DomainResolver;
use rsipstack::{dialog::dialog_layer::DialogLayer, transaction::endpoint::MessageInspector};
use rsipstack::{
    rsip::{Host, HostWithPort},
    transport::SipAddr,
};
use std::future::pending;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::{collections::HashMap, net::SocketAddr};
use std::{collections::HashSet, time::Instant};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::select;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Generates a short unique session id for incoming calls, e.g. `s.3f9a2b1c4d5e`,
/// instead of reusing the raw SIP dialog-id string. Collisions with live
/// sessions are retried (practically impossible with 48 bits of randomness).
fn generate_short_session_id(invitation: &Invitation) -> String {
    loop {
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        let session_id = format!("s.{}", &uuid);
        if !invitation.session_exists(&session_id) {
            return session_id;
        }
    }
}

/// Extracts the network source of a received request from its top Via header:
/// the IP comes from the `received` parameter (populated by our transport for
/// NAT'd peers) or the sent-by host when it is an IP literal; the hostname is
/// the sent-by host as written. Port is intentionally ignored (carrier source
/// ports change between probes).
fn extract_via_source(request: &rsipstack::rsip::Request) -> (Option<std::net::IpAddr>, String) {
    use rsipstack::rsip::ToTypedHeader;
    let Ok(via) = request.via_header().and_then(|v| v.typed()) else {
        return (None, String::new());
    };
    let source_ip = via
        .received()
        .and_then(|r| r.ok())
        .or_else(|| match &via.sent_by().host {
            rsipstack::rsip::Host::IpAddr(ip) => Some(*ip),
            _ => None,
        });
    (source_ip, via.sent_by().host.to_string())
}

pub struct AppStateInner {
    /// Live configuration snapshot, hot-swappable via
    /// [`AppStateInner::apply_config`]. Reads go through
    /// [`AppStateInner::config`]: a call captures one snapshot at construction
    /// and keeps it for its lifetime, so a reload never changes a call that is
    /// already running.
    config: ArcSwap<Config>,
    /// Path recorded at startup; used by the reload endpoint and the watcher.
    pub config_path: Option<Arc<str>>,
    pub token: CancellationToken,
    pub stream_engine: Arc<StreamEngine>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub endpoint: Endpoint,
    pub registration_handles: Mutex<HashMap<String, CancellationToken>>,
    pub alive_users: Arc<RwLock<HashSet<String>>>,
    pub dialog_layer: Arc<DialogLayer>,
    pub create_invitation_handler: Option<FnCreateInvitationHandler>,
    pub invitation: Invitation,
    pub routing_state: Arc<crate::call::RoutingState>,
    pub pending_playbooks: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    /// In-process SIP signaling capture ([`crate::sipflow`]); `None` when
    /// the `[sipflow]` section is absent/disabled.
    pub sip_flow: Option<Arc<SipFlow>>,
    pub learned_public_address: SharedPublicAddress,
    /// Peer source addresses learned from call traffic, used by the OPTIONS
    /// ACL (`[options_response]`).
    pub learned_peers: crate::useragent::peer_learning::SharedLearnedPeers,
    /// Optional hook that rewrites the custom headers an incoming REFER
    /// carries onto the transfer INVITE; `None` forwards them unchanged.
    pub refer_header_resolver: Option<std::sync::Arc<dyn ReferHeaderResolver>>,

    pub active_calls: Arc<std::sync::Mutex<HashMap<String, ActiveCallRef>>>,
    pub total_calls: AtomicU64,
    pub total_failed_calls: AtomicU64,
    pub uptime: DateTime<Local>,
    pub shutting_down: Arc<AtomicBool>,
}

pub type AppState = Arc<AppStateInner>;

pub struct AppStateBuilder {
    pub config: Option<Config>,
    pub stream_engine: Option<Arc<StreamEngine>>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub callrecord_formatter: Option<Arc<dyn CallRecordFormatter>>,
    pub cancel_token: Option<CancellationToken>,
    pub create_invitation_handler: Option<FnCreateInvitationHandler>,
    pub config_path: Option<String>,

    pub message_inspector: Option<Box<dyn MessageInspector>>,
    pub target_locator: Option<Box<dyn TargetLocator>>,
    pub transport_inspector: Option<Box<dyn TransportEventInspector>>,
    pub refer_header_resolver: Option<std::sync::Arc<dyn ReferHeaderResolver>>,
    /// Externally created signaling recorder; when set it takes precedence
    /// over `[sipflow]` config (used by embedders that need the same
    /// `SipFlow` handle in their own callrecord manager).
    pub sip_flow: Option<Arc<SipFlow>>,
}

impl AppStateInner {
    pub fn auto_learn_public_address_enabled(&self) -> bool {
        self.config().auto_learn_public_address.unwrap_or(false)
    }

    /// Whether an OPTIONS probe from `source_ip`/`source_host` (as extracted
    /// from the top Via header) should be answered: the source must match the
    /// static ACL, a registered-server host, or a peer address learned from
    /// call traffic within the configured TTL.
    pub fn options_source_allowed(
        &self,
        source_ip: Option<std::net::IpAddr>,
        source_host: &str,
    ) -> bool {
        if self.config().options_matches_static(source_ip, source_host) {
            return true;
        }
        if !self.config().options_auto_learn() {
            return false;
        }
        let ttl = self.config().options_learn_ttl();
        match source_ip {
            Some(ip) => self.learned_peers.contains_within(&ip, ttl),
            None => false,
        }
    }

    pub fn get_dump_events_file(&self, session_id: &String) -> String {
        let recorder_root = self.config().recorder_path();
        let root = Path::new(&recorder_root);
        if !root.exists() {
            match std::fs::create_dir_all(root) {
                Ok(_) => {
                    info!("created dump events root: {}", root.to_string_lossy());
                }
                Err(e) => {
                    warn!(
                        "Failed to create dump events root: {} {}",
                        e,
                        root.to_string_lossy()
                    );
                }
            }
        }
        root.join(format!("{}.events.jsonl", session_id))
            .to_string_lossy()
            .to_string()
    }

    pub fn get_recorder_file(&self, session_id: &String) -> String {
        let recorder_root = self.config().recorder_path();
        let root = Path::new(&recorder_root);
        if !root.exists() {
            match std::fs::create_dir_all(root) {
                Ok(_) => {
                    info!("created recorder root: {}", root.to_string_lossy());
                }
                Err(e) => {
                    warn!(
                        "Failed to create recorder root: {} {}",
                        e,
                        root.to_string_lossy()
                    );
                }
            }
        }
        let desired_ext = self.config().recorder_format().extension();
        let mut filename = session_id.clone();
        if !filename
            .to_lowercase()
            .ends_with(&format!(".{}", desired_ext.to_lowercase()))
        {
            filename = format!("{}.{}", filename, desired_ext);
        }
        root.join(filename).to_string_lossy().to_string()
    }

    pub async fn serve(self: Arc<Self>) -> Result<()> {
        let incoming_txs = self.endpoint.incoming_transactions()?;
        let token = self.token.child_token();
        let endpoint_inner = self.endpoint.inner.clone();
        let dialog_layer = self.dialog_layer.clone();
        let app_state_clone = self.clone();

        match self.start_registration().await {
            Ok(count) => {
                info!("registration started, count: {}", count);
            }
            Err(e) => {
                warn!("failed to start registration: {:?}", e);
            }
        }

        let pending_cleanup_state = self.clone();
        let pending_cleanup_token = token.clone();
        crate::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            let ttl = Duration::from_secs(300);
            loop {
                tokio::select! {
                    _ = pending_cleanup_token.cancelled() => break,
                    _ = interval.tick() => {
                        let mut pending = pending_cleanup_state.pending_playbooks.lock().await;
                        let before = pending.len();
                        pending.retain(|_, (_, created_at)| created_at.elapsed() < ttl);
                        let removed = before - pending.len();
                        if removed > 0 {
                            info!(removed, remaining = pending.len(), "cleaned up stale pending_playbooks entries");
                        }
                    }
                }
            }
        });

        tokio::select! {
            _ = token.cancelled() => {
                info!("cancelled");
            }
            result = endpoint_inner.serve() => {
                if let Err(e) = result {
                    info!("endpoint serve error: {:?}", e);
                }
            }
            result = app_state_clone.process_incoming_request(dialog_layer.clone(), incoming_txs) => {
                if let Err(e) = result {
                    info!("process incoming request error: {:?}", e);
                }
            },
        }

        let total_secs = self.config().graceful_shutdown_timeout.unwrap_or(30);
        let timeout = self.config()
            .graceful_shutdown
            .map(|_| Duration::from_secs(total_secs));

        match self.stop_registration(timeout).await {
            Ok(_) => {
                info!("registration stopped, waiting for clear");
            }
            Err(e) => {
                warn!("failed to stop registration: {:?}", e);
            }
        }
        info!("stopping");
        Ok(())
    }

    async fn process_incoming_request(
        self: Arc<Self>,
        dialog_layer: Arc<DialogLayer>,
        mut incoming: TransactionReceiver,
    ) -> Result<()> {
        while let Some(mut tx) = incoming.recv().await {
            let key: &rsipstack::transaction::key::TransactionKey = &tx.key;
            info!(?key, "received transaction");
            if tx.original.to_header()?.tag()?.as_ref().is_some() {
                match dialog_layer.match_dialog(&tx) {
                    Some(mut d) => {
                        crate::spawn(async move {
                            match d.handle(&mut tx).await {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("error handling transaction: {:?}", e);
                                }
                            }
                        });
                        continue;
                    }
                    None => {
                        info!("dialog not found: {}", tx.original);
                        match tx
                            .reply(rsipstack::rsip::StatusCode::CallTransactionDoesNotExist)
                            .await
                        {
                            Ok(_) => (),
                            Err(e) => {
                                info!("error replying to request: {:?}", e);
                            }
                        }
                        continue;
                    }
                }
            }
            // out dialog, new server dialog
            let (state_sender, state_receiver) = dialog_layer.new_dialog_state_channel();
            match tx.original.method {
                rsipstack::rsip::Method::Invite | rsipstack::rsip::Method::Ack => {
                    // Reject new INVITEs during graceful shutdown
                    if self.shutting_down.load(Ordering::Relaxed) {
                        info!(?key, "rejecting INVITE during graceful shutdown");
                        match tx
                            .reply_with(
                                rsipstack::rsip::StatusCode::ServiceUnavailable,
                                vec![rsipstack::rsip::Header::Other(
                                    "Reason".into(),
                                    "SIP;cause=503;text=\"Server shutting down\"".into(),
                                )],
                                None,
                            )
                            .await
                        {
                            Ok(_) => (),
                            Err(e) => {
                                info!("error replying to request: {:?}", e);
                            }
                        }
                        continue;
                    }

                    let invitation_handler = match self.create_invitation_handler {
                        Some(ref create_invitation_handler) => {
                            create_invitation_handler(self.config().handler.as_ref()).ok()
                        }
                        _ => default_create_invite_handler(
                            self.config().handler.as_ref(),
                            Some(self.clone()),
                        ),
                    };
                    let invitation_handler = match invitation_handler {
                        Some(h) => h,
                        None => {
                            info!(?key, "no invite handler configured, rejecting INVITE");
                            match tx
                                .reply_with(
                                    rsipstack::rsip::StatusCode::ServiceUnavailable,
                                    vec![rsipstack::rsip::Header::Other(
                                        "Reason".into(),
                                        "SIP;cause=503;text=\"No invite handler configured\""
                                            .into(),
                                    )],
                                    None,
                                )
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("error replying to request: {:?}", e);
                                }
                            }
                            continue;
                        }
                    };
                    let local_addr = tx
                        .connection
                        .as_ref()
                        .map(|connection| connection.get_addr().clone())
                        .or_else(|| dialog_layer.endpoint.get_addrs().first().cloned());
                    let contact_username =
                        tx.original.uri.auth.as_ref().map(|auth| auth.user.as_str());
                    let contact = local_addr.as_ref().map(|addr| {
                        build_public_contact_uri(
                            &self.learned_public_address,
                            self.auto_learn_public_address_enabled(),
                            addr,
                            contact_username,
                            None,
                        )
                    });

                    let dialog = match dialog_layer.get_or_create_server_invite(
                        &tx,
                        state_sender,
                        None,
                        contact,
                    ) {
                        Ok(d) => d,
                        Err(e) => {
                            // 481 Dialog/Transaction Does Not Exist
                            info!("failed to obtain dialog: {:?}", e);
                            match tx
                                .reply(rsipstack::rsip::StatusCode::CallTransactionDoesNotExist)
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("error replying to request: {:?}", e);
                                }
                            }
                            continue;
                        }
                    };

                    let dialog_id = dialog.id();
                    let dialog_id_str = dialog_id.to_string();
                    let session_id = if dialog_id_str.len() > 64 { generate_short_session_id(&self.invitation) } else { dialog_id_str.clone() };
                    // Register the dialog's SIP Call-ID under this session so
                    // sipflow can merge its signaling at hangup.
                    if let Some(flow) = self.sip_flow.as_ref() {
                        flow.associate(&session_id, &dialog_id.call_id);
                    }
                    let dialog_id_for_cleanup = dialog_id.clone();
                    let token = self.token.child_token();
                    let pending_dialog = PendingDialog {
                        token: token.clone(),
                        dialog: dialog.clone(),
                        state_receiver,
                    };

                    let guard = Arc::new(PendingDialogGuard::new_with_session(
                        self.invitation.clone(),
                        dialog_id,
                        session_id.clone(),
                        pending_dialog,
                    ));

                    let accept_timeout = self
                        .config()
                        .accept_timeout
                        .as_ref()
                        .and_then(|t| parse_duration(t).ok())
                        .unwrap_or_else(|| Duration::from_secs(60));

                    let mut dialog_ref = dialog.clone();
                    let routing_state = self.routing_state.clone();
                    let dialog_for_reject = dialog.clone();
                    let invitation_for_cleanup = self.invitation.clone();
                    let active_calls_for_task = self.active_calls.clone();
                    let session_id_for_task = session_id.clone();
                    crate::spawn(async move {
                        info!(
                            session_id = session_id_for_task,
                            id = dialog_id_str,
                            "incoming invite task started"
                        );
                        let _pending_guard = guard;
                        let token_ref = token.clone();
                        let accept_timeout_sleep = tokio::time::sleep(accept_timeout);
                        let invite_handler = invitation_handler.on_invite(
                            session_id_for_task.clone(),
                            token.clone(),
                            dialog.clone(),
                            routing_state,
                        );
                        let dialog_handle = dialog_ref.handle(&mut tx);
                        tokio::pin!(accept_timeout_sleep);
                        tokio::pin!(invite_handler);
                        tokio::pin!(dialog_handle);

                        let mut cancel_done = false;
                        let mut accept_timeout_done = false;
                        let mut invite_done = false;
                        loop {
                            let mut reject_request = None;

                            select! {
                                _ = token_ref.cancelled(), if !cancel_done => {
                                    cancel_done = true;
                                    reject_request = Some((
                                        rsipstack::rsip::StatusCode::ServiceUnavailable,
                                        "invite cancelled".to_string(),
                                        "cancelled",
                                    ));
                                }
                                _ = &mut accept_timeout_sleep, if !accept_timeout_done
                                    && dialog_for_reject.state().can_cancel() => {
                                    accept_timeout_done = true;
                                    reject_request = Some((
                                        rsipstack::rsip::StatusCode::RequestTimeout,
                                        "accept timeout".to_string(),
                                        "accept timeout",
                                    ));
                                }
                                result = &mut invite_handler, if !invite_done => {
                                    invite_done = true;
                                    match result {
                                        Ok(_) => {
                                            info!(id = dialog_id_str, "invite handler completed");
                                        }
                                        Err(e) => {
                                            info!(id = dialog_id_str, "error handling invite: {:?}", e);
                                            reject_request = Some((
                                                rsipstack::rsip::StatusCode::ServiceUnavailable,
                                                format!("Failed to process invite: {}", e),
                                                "invite handler error",
                                            ));
                                        }
                                    }
                                }
                                result = &mut dialog_handle => {
                                    match result {
                                        Ok(_) => {
                                            info!(id = dialog_id_str, "dialog handling finished");
                                        }
                                        Err(e) => {
                                            info!(
                                                id = dialog_id_str,
                                                "dialog handling ended with error: {:?}", e
                                            );
                                        }
                                    }
                                    if let rsipstack::dialog::dialog::DialogState::Terminated(
                                        _,
                                        reason,
                                    ) = dialog_for_reject.state()
                                    {
                                        info!(
                                            id = dialog_id_str,
                                            "terminated invite dialog finished, cancelling invite token"
                                        );
                                        token_ref.cancel();

                                        // A websocket client may already have
                                        // attached an ActiveCall for this session,
                                        // but that call only starts watching the
                                        // dialog state on its first command
                                        // (Ringing/Accept). A far-end CANCEL before
                                        // that command would otherwise leave the
                                        // call in `active_calls` forever. Propagate
                                        // the termination: report the hangup and
                                        // tear the call down.
                                        let maybe_call = active_calls_for_task
                                            .lock()
                                            .unwrap()
                                            .get(&session_id_for_task)
                                            .cloned();
                                        if let Some(call) = maybe_call {
                                            let has_reason = call
                                                .progress
                                                .load_full()
                                                .hangup_reason
                                                .is_some();
                                            if !has_reason {
                                                let term =
                                                    crate::call::state::CallProgress::termination(
                                                        Some(&reason),
                                                    );
                                                call.leg().update_progress(|p| {
                                                    p.last_status_code = term.status_code;
                                                    p.set_hangup_reason(
                                                        term.hangup_reason.clone(),
                                                    );
                                                });
                                                let event = call.leg().build_hangup_event(
                                                    call.session_id.clone(),
                                                    Some(term.initiator.to_string()),
                                                );
                                                call.event_sender.send(event).ok();
                                            }
                                            call.cancel_token.cancel();
                                        }
                                    }
                                    info!(id = dialog_id_str, "incoming invite task finished");
                                    break;
                                }
                            }

                            if let Some((code, reason, source)) = reject_request {
                                if dialog_for_reject.state().can_cancel() {
                                    info!(
                                        id = dialog_id_str,
                                        ?code,
                                        %reason,
                                        source,
                                        "rejecting invite"
                                    );
                                    if let Err(e) =
                                        dialog_for_reject.reject(Some(code), Some(reason))
                                    {
                                        info!(
                                            id = dialog_id_str,
                                            "error rejecting invite: {:?}", e
                                        );
                                    }
                                    invitation_for_cleanup.get_pending_call(&dialog_id_for_cleanup);
                                    invitation_for_cleanup
                                        .dialog_layer
                                        .remove_dialog(&dialog_id_for_cleanup);
                                }
                            }
                        }
                    });
                }
                rsipstack::rsip::Method::Options => {
                    if self.config().options_response_enabled() {
                        let (source_ip, source_host) = extract_via_source(&tx.original);
                        if self.options_source_allowed(source_ip, &source_host) {
                            info!(?key, %source_host, "responding to out-of-dialog OPTIONS request");
                            let mut headers = vec![
                                {
                                    let allow_header: rsipstack::rsip::Header =
                                        typed::Allow::from(Method::all()).into();
                                    allow_header
                                },
                                rsipstack::rsip::Header::Accept(Accept::new("application/sdp")),
                            ];
                            let extra_headers = self.config().options_extra_headers();
                            if !extra_headers.is_empty() {
                                headers.extend(crate::sip_util::sip_headers_from_map(
                                    &extra_headers.into_iter().collect::<HashMap<_, _>>(),
                                ));
                            }
                            match tx
                                .reply_with(rsipstack::rsip::StatusCode::OK, headers, None)
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("error replying to OPTIONS: {:?}", e);
                                }
                            }
                        } else {
                            debug!(
                                ?key,
                                %source_host,
                                "dropping OPTIONS probe from non-allowed source"
                            );
                        }
                    } else {
                        info!(?key, "ignoring out-of-dialog OPTIONS request");
                    }
                    continue;
                }
                rsipstack::rsip::Method::Refer => {
                    info!(?key, "ignoring out-of-dialog REFER");
                    match tx.reply(rsipstack::rsip::StatusCode::BadRequest).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error replying to out-of-dialog REFER: {:?}", e);
                        }
                    }
                    continue;
                }
                _ => {
                    info!(?key, "received request: {:?}", tx.original.method);
                    match tx.reply(rsipstack::rsip::StatusCode::OK).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error replying to request: {:?}", e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn stop(&self) {
        if self.shutting_down.swap(true, Ordering::Relaxed) {
            return;
        }
        info!("stopping, marking as shutting down");
        self.token.cancel();
    }

    pub async fn graceful_stop(&self, total_timeout_secs: u64) -> Result<()> {
        if self.shutting_down.swap(true, Ordering::Relaxed) {
            return Ok(());
        }

        info!("graceful stopping, marking as shutting down");
        let timeout = Duration::from_secs(total_timeout_secs);

        let (reg_result, ()) = tokio::join!(
            self.stop_registration(Some(timeout)),
            self.wait_for_active_calls(timeout),
        );
        if let Err(e) = reg_result {
            warn!("stop_registration error: {}", e);
        }
        self.token.cancel();
        Ok(())
    }

    async fn wait_for_active_calls(&self, timeout: Duration) {
        let active_calls = self.active_calls.clone();
        let check_loop = async move {
            let mut last_count = usize::MAX;
            loop {
                let count = active_calls.lock().unwrap().len();
                if count == 0 {
                    break;
                }
                if count != last_count {
                    info!(active_calls = count, "waiting for active calls to finish");
                    last_count = count;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        match tokio::time::timeout(timeout, check_loop).await {
            Ok(()) => info!("all active calls finished"),
            Err(_) => warn!("timed out waiting for active calls to finish, forcing shutdown"),
        }
    }

    pub async fn start_registration(&self) -> Result<usize> {
        let mut count = 0;
        let config = self.config();
        if let Some(register_users) = &config.register_users {
            for option in register_users.iter() {
                if option.disabled.unwrap_or(false) {
                    info!(user = option.aor(), "registration disabled, skipping");
                    continue;
                }
                match self.register(option.clone()).await {
                    Ok(_) => {
                        count += 1;
                    }
                    Err(e) => {
                        warn!("failed to register user: {:?} {:?}", e, option);
                    }
                }
            }
        }
        Ok(count)
    }

    pub fn find_credentials_for_callee(&self, callee: &str) -> Option<UserCredential> {
        let callee_uri = crate::sip_util::ensure_sip_scheme(callee.to_string());

        let parsed_callee = match rsipstack::rsip::Uri::try_from(callee_uri.as_str()) {
            Ok(uri) => uri,
            Err(e) => {
                warn!("failed to parse callee URI: {} {:?}", callee, e);
                return None;
            }
        };

        // Matching is host-kind-homogeneous: a domain callee matches domain
        // servers, an IP callee matches IP servers.
        match &parsed_callee.host_with_port.host {
            rsipstack::rsip::Host::Domain(domain) => {
                let domain = domain.0.clone();
                self.find_credentials(
                    callee,
                    &format!("domain {}", domain),
                    |host| matches!(host, rsipstack::rsip::Host::Domain(d) if d.0 == domain),
                )
            }
            rsipstack::rsip::Host::IpAddr(ip) => {
                let ip = *ip;
                self.find_credentials(callee, "IP match", move |host| {
                    matches!(host, rsipstack::rsip::Host::IpAddr(server_ip) if *server_ip == ip)
                })
            }
        }
    }

    /// Scan registered users for the first server matching `pred` that has a
    /// credential.
    fn find_credentials(
        &self,
        callee: &str,
        match_type: &str,
        pred: impl Fn(&rsipstack::rsip::Host) -> bool,
    ) -> Option<UserCredential> {
        let config = self.config();
        let register_users = config.register_users.as_ref()?;
        for option in register_users.iter() {
            let server = crate::sip_util::ensure_sip_scheme(option.server.clone());

            let parsed_server = match rsipstack::rsip::Uri::try_from(server.as_str()) {
                Ok(uri) => uri,
                Err(e) => {
                    warn!("failed to parse server URI: {} {:?}", option.server, e);
                    continue;
                }
            };

            if pred(&parsed_server.host_with_port.host) {
                if let Some(cred) = &option.credential {
                    info!(
                        callee,
                        username = cred.username,
                        server = option.server,
                        match_type,
                        "Auto-injecting credentials from registered user for outbound call"
                    );
                    return Some(cred.clone());
                }
            }
        }
        None
    }

    pub async fn stop_registration(&self, wait_for_clear: Option<Duration>) -> Result<()> {
        {
            let mut handles = self.registration_handles.lock().await;
            for (_, cancel_token) in handles.drain() {
                cancel_token.cancel();
            }
        }

        if let Some(duration) = wait_for_clear {
            let live_users = self.alive_users.clone();
            let check_loop = async move {
                loop {
                    let is_empty = {
                        let users = live_users
                            .read()
                            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
                        users.is_empty()
                    };
                    if is_empty {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Ok::<(), anyhow::Error>(())
            };
            match tokio::time::timeout(duration, check_loop).await {
                Ok(_) => {}
                Err(e) => {
                    warn!("failed to wait for clear: {}", e);
                    return Err(anyhow::anyhow!("failed to wait for clear: {}", e));
                }
            }
        }
        Ok(())
    }

    pub async fn register(&self, option: RegisterOption) -> Result<()> {
        let user = option.aor();
        let server = crate::sip_util::ensure_sip_scheme(option.server.clone());
        let sip_server = match rsipstack::rsip::Uri::try_from(server) {
            Ok(uri) => uri,
            Err(e) => {
                warn!("failed to parse server: {} {:?}", e, option.server);
                return Err(anyhow::anyhow!("failed to parse server: {}", e));
            }
        };
        let cancel_token = self.token.child_token();
        let credential = option.credential.clone().map(|c| c.into());
        let registration = rsipstack::dialog::registration::Registration::new(
            self.endpoint.inner.clone(),
            credential,
        );
        let mut handle = RegistrationHandle {
            registration,
            option,
            cancel_token: cancel_token.clone(),
            start_time: Instant::now(),
            last_update: Instant::now(),
            last_response: None,
        };
        self.registration_handles
            .lock()
            .await
            .insert(user.clone(), cancel_token);
        tracing::debug!(user = user.as_str(), "starting registration task");
        let alive_users = self.alive_users.clone();

        crate::spawn(async move {
            handle.start_time = Instant::now();
            let cancel_token = handle.cancel_token.clone();
            let addrs = handle.registration.endpoint.get_addrs();
            let local_bind_addr = if let Some(addr) = find_local_addr_for_uri(&addrs, &sip_server) {
                addr
            } else {
                warn!(
                    user = user.as_str(),
                    server = %sip_server,
                    "failed to get local bind address for registration transport"
                );
                alive_users.write().unwrap().remove(&user);
                return;
            };
            let user = handle.option.aor();
            alive_users.write().unwrap().remove(&user);
            let mut contact_address = local_bind_addr.addr.clone();
            let mut contact = build_contact(
                &local_bind_addr,
                Some(contact_address.clone()),
                Some(handle.option.username.as_str()),
                None,
            );
            let mut should_register = true;
            let mut timer = pending().boxed();

            loop {
                select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    _ = timer.as_mut(), if !should_register => {
                        should_register = true;
                        timer = Box::pin(pending());
                    }
                    result = handle.do_register(&sip_server, None, &contact), if should_register => {
                        match result {
                            Ok((expires, new_addr)) => {
                                if handle
                                    .should_retry_registration_now(
                                        &local_bind_addr,
                                        &contact_address,
                                        new_addr.as_ref(),
                                    )
                                {
                                    if let Some(next_contact_address) = new_addr {
                                        info!(
                                            user = user.as_str(),
                                            current_contact = %contact_address,
                                            next_contact = %next_contact_address,
                                            "public address changed, retrying registration immediately",
                                        );
                                        contact_address = next_contact_address;
                                        contact = build_contact(
                                            &local_bind_addr,
                                            Some(contact_address.clone()),
                                            Some(handle.option.username.as_str()),
                                            None,
                                        );
                                        continue;
                                    }
                                }
                                info!(
                                    user = user.as_str(),
                                    expires = expires,
                                    contact = %contact_address,
                                    alive_users = alive_users.read().unwrap().len(),
                                    "registration refreshed",
                                );
                                alive_users.write().unwrap().insert(user.clone());
                                should_register = false;
                                timer = Box::pin(tokio::time::sleep(Duration::from_secs(
                                    (expires * 3 / 4) as u64,
                                )));
                            }
                            Err(e) => {
                                warn!(
                                    user = user.as_str(),
                                    alive_users = alive_users.read().unwrap().len(),
                                    "registration failed: {:?}", e
                                );
                                should_register = false;
                                timer = Box::pin(tokio::time::sleep(Duration::from_secs(60)));
                            }
                        }
                    }
                }
            }
            handle
                .do_register(&sip_server, Some(0), &contact)
                .await
                .ok();
            alive_users.write().unwrap().remove(&user);
        });
        Ok(())
    }
}

impl AppStateInner {
    /// Current config snapshot. Cheap (`Arc` clone); call sites that make
    /// several decisions from one consistent view should bind it first.
    #[inline]
    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
    }

    /// Path of the config file this process was started with, if any.
    pub fn config_path(&self) -> Option<&str> {
        self.config_path.as_deref()
    }

    /// Re-read the config file recorded at startup and apply it.
    pub async fn reload_from_disk(&self) -> Result<ReloadReport, ReloadError> {
        let Some(path) = self.config_path() else {
            return Err(ReloadError::NoPath);
        };
        // `Config::load` also re-applies env overrides, so the precedence
        // (env > file > default) is identical to startup.
        let proposed = Config::load(path)
            .map_err(|e| ReloadError::Invalid(format!("{path}: {e}")))?;
        Ok(self.apply_config(proposed))
    }

    /// Validate a proposed snapshot without installing it (`mode=check`).
    pub fn preview_config(&self, proposed: Config) -> ReloadReport {
        self.diff_report(&proposed)
    }

    /// Validate and install a new config snapshot.
    ///
    /// Applying is atomic: the snapshot is swapped in one store, after which
    /// the module switches and the media cache directory are re-derived. The
    /// report classifies every changed top-level section so the caller can see
    /// which changes are live (`applied`), which wait for the next call
    /// (`deferred`) and which need a restart (`requires_restart`).
    pub fn apply_config(&self, proposed: Config) -> ReloadReport {
        let report = self.diff_report(&proposed);
        if report.status == "error" {
            return report;
        }

        // Swap first, then derive the side effects from the new snapshot so a
        // concurrent call sees either the old or the new world, never a mix.
        self.config.store(Arc::new(proposed));
        let current = self.config.load_full();
        apply_side_effects(&current);
        report
    }

    fn diff_report(&self, proposed: &Config) -> ReloadReport {
        diff_configs(&self.config.load_full(), proposed)
    }
}

/// Classify the differences between two snapshots into the reload report.
fn diff_configs(old: &Config, proposed: &Config) -> ReloadReport {
    let old_json = serde_json::to_value(old).unwrap_or_default();
    let new_json = serde_json::to_value(proposed).unwrap_or_default();

    let mut applied = Vec::new();
    let mut deferred = Vec::new();
    let mut requires_restart = Vec::new();
    let (Some(old_map), Some(new_map)) = (old_json.as_object(), new_json.as_object()) else {
        return ReloadReport {
            status: "error",
            revision: String::new(),
            applied,
            deferred,
            requires_restart,
        };
    };

    let mut keys: Vec<&String> = old_map.keys().chain(new_map.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let before = old_map.get(key).unwrap_or(&serde_json::Value::Null);
        let after = new_map.get(key).unwrap_or(&serde_json::Value::Null);
        if before == after {
            continue;
        }
        match classify_field(key) {
            FieldClass::Hot => applied.push(key.clone()),
            FieldClass::Deferred => deferred.push(key.clone()),
            FieldClass::Restart => requires_restart.push(key.clone()),
        }
    }

    ReloadReport {
        status: if applied.is_empty() && deferred.is_empty() && requires_restart.is_empty() {
            "unchanged"
        } else {
            "ok"
        },
        revision: chrono::Utc::now().to_rfc3339(),
        applied,
        deferred,
        requires_restart,
    }
}

/// How a changed top-level config section takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldClass {
    /// Read live from the snapshot (per request / per admission check).
    Hot,
    /// Read when a call, track or upload is set up.
    Deferred,
    /// Captured at build time; only a process restart applies it.
    Restart,
}

fn classify_field(key: &str) -> FieldClass {
    match key {
        // Live per request/admission check.
        "admission" | "modules" | "reload" | "auto_learn_public_address"
        | "enable_options_response" | "options_response" | "peers" | "rewrites"
        | "trunk_rules" | "http_access_skip_paths" => FieldClass::Hot,
        // Read when a call/track/upload is set up.
        "callrecord" | "recording" | "media_cache_path" | "codecs" | "ice_servers"
        | "ambiance" | "jitter_buffer" | "enable_rtp_latching" | "enable_ice_lite"
        | "ice_disconnect_grace" | "enable_srtp" | "external_ip" | "rtp_bind_ip"
        | "auto_refer" | "auto_refer_timeout" | "accept_timeout" => FieldClass::Deferred,
        // Listener/UA/allocator/engine wiring built at startup.
        _ => FieldClass::Restart,
    }
}

/// Re-derive the process-wide state that mirrors the config snapshot.
fn apply_side_effects(config: &Config) {
    crate::runtime::apply_config(config);
    if let Err(e) = crate::media::cache::set_cache_dir(&config.media_cache_path) {
        tracing::warn!(error = %e, "failed to update media cache dir on reload");
    }
}

/// Outcome of one reload attempt.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReloadReport {
    /// `ok` (changed), `unchanged`, or `error`.
    pub status: &'static str,
    /// RFC 3339 timestamp of the snapshot this report describes.
    pub revision: String,
    /// Live immediately.
    pub applied: Vec<String>,
    /// Applies from the next call onwards.
    pub deferred: Vec<String>,
    /// Requires a process restart.
    pub requires_restart: Vec<String>,
}

#[derive(Debug)]
pub enum ReloadError {
    /// The process was started without a config file path.
    NoPath,
    /// The file could not be read or parsed; the old snapshot stays live.
    Invalid(String),
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReloadError::NoPath => write!(
                f,
                "no config path recorded at startup; start with --conf or with_config_metadata"
            ),
            ReloadError::Invalid(e) => write!(f, "invalid configuration: {e}"),
        }
    }
}

impl std::error::Error for ReloadError {}

impl Drop for AppStateInner {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SimpleDomainResolver;

#[async_trait]
impl DomainResolver for SimpleDomainResolver {
    async fn resolve(&self, target: &SipAddr) -> rsipstack::Result<SipAddr> {
        match &target.addr.host {
            Host::Domain(domain) => {
                let port: u16 = target.addr.port.map(|p| p.value()).unwrap_or(5060);
                let addr_str = format!("{}:{}", domain, port);
                match tokio::net::lookup_host(&addr_str).await {
                    Ok(mut addrs) => {
                        if let Some(addr) = addrs.next() {
                            Ok(SipAddr {
                                r#type: target.r#type,
                                addr: HostWithPort {
                                    host: Host::IpAddr(addr.ip()),
                                    port: Some(rsipstack::rsip::Port(addr.port())),
                                },
                            })
                        } else {
                            Err(rsipstack::Error::DnsResolutionError(format!(
                                "no addresses found for {}",
                                domain
                            )))
                        }
                    }
                    Err(e) => Err(rsipstack::Error::DnsResolutionError(format!(
                        "DNS resolution failed for {}: {}",
                        domain, e
                    ))),
                }
            }
            _ => Ok(target.clone()),
        }
    }
}

impl AppStateBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            stream_engine: None,
            callrecord_sender: None,
            callrecord_formatter: None,
            cancel_token: None,
            create_invitation_handler: None,
            config_path: None,
            message_inspector: None,
            target_locator: None,
            transport_inspector: None,
            refer_header_resolver: None,
            sip_flow: None,
        }
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_stream_engine(mut self, stream_engine: Arc<StreamEngine>) -> Self {
        self.stream_engine = Some(stream_engine);
        self
    }

    pub fn with_callrecord_sender(mut self, sender: CallRecordSender) -> Self {
        self.callrecord_sender = Some(sender);
        self
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn with_config_metadata(mut self, path: Option<String>) -> Self {
        self.config_path = path;
        self
    }

    pub fn with_inspector(&mut self, inspector: Box<dyn MessageInspector>) -> &mut Self {
        self.message_inspector = Some(inspector);
        self
    }
    pub fn with_target_locator(&mut self, locator: Box<dyn TargetLocator>) -> &mut Self {
        self.target_locator = Some(locator);
        self
    }

    pub fn with_transport_inspector(
        &mut self,
        inspector: Box<dyn TransportEventInspector>,
    ) -> &mut Self {
        self.transport_inspector = Some(inspector);
        self
    }

    /// Install a hook that rewrites the custom headers an incoming REFER
    /// carries onto the transfer INVITE. Without one the REFER headers are
    /// forwarded unchanged.
    pub fn with_refer_header_resolver(
        mut self,
        resolver: std::sync::Arc<dyn ReferHeaderResolver>,
    ) -> Self {
        self.refer_header_resolver = Some(resolver);
        self
    }

    /// Inject an externally created signaling recorder (see
    /// [`crate::sipflow`]). Takes precedence over `[sipflow]` config.
    pub fn with_sipflow(mut self, sip_flow: Arc<SipFlow>) -> Self {
        self.sip_flow = Some(sip_flow);
        self
    }

    pub async fn build(mut self) -> Result<AppState> {
        let config: Arc<Config> = Arc::new(self.config.unwrap_or_default());
        // Publish the module switches before the first call can be set up.
        crate::runtime::apply_config(&config);
        let token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let _ = set_cache_dir(&config.media_cache_path);
        let local_ip = if !config.addr.is_empty() {
            std::net::IpAddr::from_str(config.addr.as_str())?
        } else {
            crate::net_tool::get_first_non_loopback_interface()?
        };
        let transport_layer = match catch_unwind(AssertUnwindSafe(|| {
            rsipstack::transport::TransportLayer::new(token.clone())
        })) {
            Ok(tl) => tl,
            Err(_) => {
                warn!(
                    "failed to initialize default DNS resolver with hickory-resolver, falling back to simple resolver via tokio::net::lookup_host"
                );
                rsipstack::transport::TransportLayer::new_with_domain_resolver(
                    token.clone(),
                    Box::new(SimpleDomainResolver),
                )
            }
        };
        let local_addr: SocketAddr = format!("{}:{}", local_ip, config.udp_port).parse()?;

        // Create UDP socket with SO_REUSEPORT for graceful restarts
        #[cfg(unix)]
        let std_socket = {
            use socket2::{Domain, Protocol, SockAddr, Socket, Type};

            let domain = if local_addr.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
                .map_err(|err| anyhow::anyhow!("Failed to create UDP socket: {}", err))?;

            socket
                .set_reuse_address(true)
                .map_err(|err| anyhow::anyhow!("Failed to set SO_REUSEADDR: {}", err))?;

            // SO_REUSEPORT (Linux/BSD)
            #[cfg(not(any(target_os = "solaris", target_os = "illumos", target_os = "cygwin")))]
            socket
                .set_reuse_port(true)
                .map_err(|err| anyhow::anyhow!("Failed to set SO_REUSEPORT: {}", err))?;

            socket
                .bind(&SockAddr::from(local_addr))
                .map_err(|err| anyhow::anyhow!("Failed to bind UDP socket: {}", err))?;

            let std_socket: std::net::UdpSocket = socket.into();
            std_socket
        };

        #[cfg(not(unix))]
        let std_socket = std::net::UdpSocket::bind(local_addr)?;

        std_socket.set_nonblocking(true)?;
        let tokio_socket = tokio::net::UdpSocket::from_std(std_socket)?;
        // Use the actual bound address (important when port=0 lets OS assign a port)
        let actual_addr = tokio_socket.local_addr()?;
        let bind_addr = rsipstack::transport::SipConnection::resolve_bind_address(actual_addr);
        let mut learned_public_address: SharedPublicAddress =
            Arc::new(ArcSwap::from_pointee(bind_addr.into()));
        let learned_peers =
            SharedLearnedPeers::new(crate::useragent::peer_learning::LEARNED_PEERS_CAPACITY);

        let udp_inner = rsipstack::transport::udp::UdpInner {
            conn: std::sync::Arc::new(tokio_socket),
            addr: rsipstack::transport::SipAddr {
                r#type: Some(rsipstack::rsip::transport::Transport::Udp),
                addr: bind_addr.into(),
            },
        };

        let external = config
            .external_ip
            .as_ref()
            .map(|ip| {
                format!("{}:{}", ip, actual_addr.port())
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse external address: {}", e))
            })
            .transpose()?;

        let udp_conn = rsipstack::transport::udp::UdpConnection::attach(
            udp_inner,
            external,
            Some(token.child_token()),
        )
        .await;

        info!(
            "start useragent, addr: {} (SO_REUSEPORT enabled)",
            udp_conn.get_addr()
        );

        transport_layer.add_transport(udp_conn.into());

        // Optional SIP over TLS transport
        if let Some(tls_port) = config.tls_port {
            let tls_addr: std::net::SocketAddr = format!("{}:{}", local_ip, tls_port).parse()?;
            let mut tls_cfg = rsipstack::transport::tls::TlsConfig::default();
            if let Some(ref cert_path) = config.tls_cert_file {
                tls_cfg.cert = Some(
                    std::fs::read(cert_path)
                        .map_err(|e| anyhow::anyhow!("tls_cert_file: {}", e))?,
                );
            }
            if let Some(ref key_path) = config.tls_key_file {
                tls_cfg.key = Some(
                    std::fs::read(key_path).map_err(|e| anyhow::anyhow!("tls_key_file: {}", e))?,
                );
            }
            let external_tls_addr = config
                .external_ip
                .as_ref()
                .and_then(|ip| format!("{}:{}", ip, tls_port).parse().ok());
            match rsipstack::transport::tls::TlsListenerConnection::new(
                tls_addr,
                external_tls_addr,
                tls_cfg,
            )
            .await
            {
                Ok(tls_conn) => {
                    transport_layer.add_transport(tls_conn.into());
                    info!("TLS SIP transport started on {}:{}", local_ip, tls_port);
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to start TLS SIP transport: {}", e));
                }
            }
        }

        let endpoint_option = rsipstack::transaction::endpoint::EndpointOption::default();
        let mut endpoint_builder = rsipstack::EndpointBuilder::new();
        if let Some(ref user_agent) = config.useragent {
            endpoint_builder.with_user_agent(user_agent.as_str());
        }

        let mut endpoint_builder = endpoint_builder
            .with_cancel_token(token.child_token())
            .with_transport_layer(transport_layer)
            .with_option(endpoint_option);

        // Inspector chain (outermost runs first): public-address learning
        // (opt-in) -> call-traffic peer learning (for the OPTIONS ACL) ->
        // any caller-provided inspector.
        let mut next_inspector = self.message_inspector.take();
        if config.options_auto_learn() {
            info!("learning call-traffic peer addresses for OPTIONS ACL");
            next_inspector = Some(Box::new(PeerAddressLearner::new_with_next(
                learned_peers.clone(),
                next_inspector,
            )));
        }
        if config.auto_learn_public_address.unwrap_or_default() {
            let inspector = LearningMessageInspector::new(bind_addr.into(), next_inspector);
            learned_public_address = inspector.shared_public_address();
            next_inspector = Some(Box::new(inspector));
        }

        // [sipflow] in-process signaling capture — outermost inspector so
        // every message (both directions, REFER transfer dialogs included)
        // is recorded before any other inspector sees it. An injected
        // recorder takes precedence over the config section.
        let sip_flow = match self.sip_flow.take() {
            Some(flow) => Some(flow),
            None => match config.sipflow.as_ref().filter(|c| c.enabled()) {
                Some(sipflow_config) => {
                    info!(
                        max_calls = sipflow_config.max_calls(),
                        max_items_per_call = sipflow_config.max_items_per_call(),
                        upload = sipflow_config.upload.is_some(),
                        "sipflow signaling capture enabled"
                    );
                    Some(Arc::new(SipFlow::new(
                        sipflow_config.clone(),
                        bind_addr.to_string(),
                        None,
                    )))
                }
                None => None,
            },
        };
        if let Some(flow) = sip_flow.as_ref() {
            flow.set_next(next_inspector.take());
            next_inspector = Some(Box::new(SipFlow::clone(flow)));
        }

        if let Some(inspector) = next_inspector {
            endpoint_builder = endpoint_builder.with_inspector(inspector);
        }
        if let Some(locator) = self.target_locator {
            endpoint_builder.with_target_locator(locator);
        } else if let Some(ref rules) = config.rewrites {
            endpoint_builder
                .with_target_locator(Box::new(RewriteTargetLocator::new(rules.clone())));
        }

        if let Some(inspector) = self.transport_inspector {
            endpoint_builder = endpoint_builder.with_transport_inspector(inspector);
        }

        let endpoint = endpoint_builder.build();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));

        let stream_engine = self.stream_engine.unwrap_or_default();

        let callrecord_formatter = if let Some(formatter) = self.callrecord_formatter {
            formatter
        } else {
            let formatter = if let Some(ref callrecord) = config.callrecord {
                DefaultCallRecordFormatter::new_with_config(callrecord)
            } else {
                DefaultCallRecordFormatter::default()
            };
            Arc::new(formatter)
        };

        let callrecord_sender = if let Some(sender) = self.callrecord_sender {
            Some(sender)
        } else if let Some(ref callrecord) = config.callrecord {
            let builder = CallRecordManagerBuilder::new()
                .with_cancel_token(token.child_token())
                .with_config(callrecord.clone())
                .with_max_concurrent(32)
                .with_formatter(callrecord_formatter.clone());
            // Attach the sipflow upload/eviction hook when capture is on.
            let builder = match sip_flow.as_ref() {
                Some(flow) => builder.with_hook(Arc::new(SipFlowUploadHook {
                    flow: flow.clone(),
                    upload: config
                        .sipflow
                        .as_ref()
                        .and_then(|c| c.upload.clone()),
                })),
                None => builder,
            };

            let mut callrecord_manager = builder.build();
            let sender = callrecord_manager.sender.clone();
            crate::spawn(async move {
                callrecord_manager.serve().await;
            });
            Some(sender)
        } else {
            None
        };

        let app_state = Arc::new(AppStateInner {
            config: ArcSwap::from(config),
            config_path: self.config_path.map(Arc::from),
            token,
            stream_engine,
            callrecord_sender,
            endpoint,
            registration_handles: Mutex::new(HashMap::new()),
            alive_users: Arc::new(RwLock::new(HashSet::new())),
            dialog_layer: dialog_layer.clone(),
            create_invitation_handler: self.create_invitation_handler,
            invitation: Invitation::new(dialog_layer),
            routing_state: Arc::new(crate::call::RoutingState::new()),
            pending_playbooks: Arc::new(Mutex::new(HashMap::new())),
            sip_flow,
            learned_public_address,
            learned_peers,
            refer_header_resolver: self.refer_header_resolver,
            active_calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
            total_calls: AtomicU64::new(0),
            total_failed_calls: AtomicU64::new(0),
            uptime: Local::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
        });

        if app_state.config().reload_watch() {
            crate::spawn(watch_config_file(app_state.clone()));
        }

        Ok(app_state)
    }
}

/// Poll the config file (mtime + size) and apply changes.
///
/// Off unless `[reload] watch = true`: deploys should use the explicit
/// `mode=check` → `mode=apply` flow. The watcher exists for ops convenience;
/// write the file atomically (`.tmp` + rename) to avoid parsing a half-written
/// config — a parse failure keeps the previous snapshot live and the next
/// change re-triggers it anyway.
async fn watch_config_file(state: AppState) {
    fn stamp(path: &str) -> Option<(std::time::SystemTime, u64)> {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    let Some(path) = state.config_path().map(str::to_string) else {
        return;
    };
    let mut last = stamp(&path);
    loop {
        tokio::time::sleep(state.config().reload_watch_interval()).await;
        if state.token.is_cancelled() {
            return;
        }
        let now = stamp(&path);
        if now == last {
            continue;
        }
        last = now;
        // Let an in-progress write settle before parsing.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        match state.reload_from_disk().await {
            Ok(report) => info!(
                status = report.status,
                applied = ?report.applied,
                deferred = ?report.deferred,
                requires_restart = ?report.requires_restart,
                "config reloaded by file watcher"
            ),
            Err(e) => warn!(error = %e, "config reload failed; keeping previous snapshot"),
        }
    }
}

#[cfg(test)]
mod reload_tests {
    use super::diff_configs;
    use crate::config::{AdmissionConfig, Config, ModuleToggle, ModulesConfig};

    #[test]
    fn identical_config_is_unchanged() {
        let old = Config::default();
        let report = diff_configs(&old, &old);
        assert_eq!(report.status, "unchanged");
        assert!(report.applied.is_empty());
        assert!(report.deferred.is_empty());
        assert!(report.requires_restart.is_empty());
    }

    #[test]
    fn hot_sections_are_live_and_listener_changes_need_restart() {
        let old = Config::default();
        let mut new = Config::default();
        new.admission = Some(AdmissionConfig {
            max_active_calls: Some(10),
            wait_ms: Some(250),
        });
        new.modules = Some(ModulesConfig {
            denoise: Some(ModuleToggle { enabled: false }),
            ..Default::default()
        });
        new.udp_port = old.udp_port + 1;

        let report = diff_configs(&old, &new);
        assert_eq!(report.status, "ok");
        assert!(report.applied.contains(&"admission".to_string()));
        assert!(report.applied.contains(&"modules".to_string()));
        assert!(report.requires_restart.contains(&"udp_port".to_string()));
        assert!(report.deferred.is_empty());
    }

    #[test]
    fn per_call_sections_are_deferred() {
        let old = Config::default();
        let mut new = Config::default();
        new.codecs = Some(vec!["pcmu".to_string()]);
        let report = diff_configs(&old, &new);
        assert_eq!(report.deferred, vec!["codecs".to_string()]);
        assert!(report.applied.is_empty());
    }

    #[test]
    fn removing_a_section_is_reported() {
        let mut old = Config::default();
        old.admission = Some(AdmissionConfig {
            max_active_calls: Some(10),
            wait_ms: None,
        });
        let new = Config::default();
        let report = diff_configs(&old, &new);
        assert!(report.applied.contains(&"admission".to_string()));
    }
}
