use crate::{
    call::{ActiveCallRef, sip::Invitation},
    callrecord::{
        CallRecordFormatter, CallRecordManagerBuilder, CallRecordSender, DefaultCallRecordFormatter,
    },
    config::Config,
    locator::RewriteTargetLocator,
    useragent::{
        RegisterOption,
        invitation::{
            FnCreateInvitationHandler, PendingDialog, PendingDialogGuard,
            default_create_invite_handler,
        },
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
use chrono::{DateTime, Local};
use futures::FutureExt;
use humantime::parse_duration;
use rsip::prelude::HeadersExt;
use rsipstack::transaction::{
    Endpoint, TransactionReceiver,
    endpoint::{TargetLocator, TransportEventInspector},
};
use rsipstack::{dialog::dialog_layer::DialogLayer, transaction::endpoint::MessageInspector};
use std::future::pending;
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
use tracing::{info, warn};

pub struct AppStateInner {
    pub config: Arc<Config>,
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
    pub learned_public_address: SharedPublicAddress,

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
}

impl AppStateInner {
    pub fn auto_learn_public_address_enabled(&self) -> bool {
        self.config.auto_learn_public_address.unwrap_or(false)
    }

    pub fn get_dump_events_file(&self, session_id: &String) -> String {
        let recorder_root = self.config.recorder_path();
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
        let recorder_root = self.config.recorder_path();
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
        let desired_ext = self.config.recorder_format().extension();
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

        // Wait for registration to stop, if not stopped within 50 seconds,
        // force stop it.
        let timeout = self
            .config
            .graceful_shutdown
            .map(|_| Duration::from_secs(10));

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
                            .reply(rsip::StatusCode::CallTransactionDoesNotExist)
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
                rsip::Method::Invite | rsip::Method::Ack => {
                    // Reject new INVITEs during graceful shutdown
                    if self.shutting_down.load(Ordering::Relaxed) {
                        info!(?key, "rejecting INVITE during graceful shutdown");
                        match tx
                            .reply_with(
                                rsip::StatusCode::ServiceUnavailable,
                                vec![rsip::Header::Other(
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
                            create_invitation_handler(self.config.handler.as_ref()).ok()
                        }
                        _ => default_create_invite_handler(
                            self.config.handler.as_ref(),
                            Some(self.clone()),
                        ),
                    };
                    let invitation_handler = match invitation_handler {
                        Some(h) => h,
                        None => {
                            info!(?key, "no invite handler configured, rejecting INVITE");
                            match tx
                                .reply_with(
                                    rsip::StatusCode::ServiceUnavailable,
                                    vec![rsip::Header::Other(
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
                                .reply(rsip::StatusCode::CallTransactionDoesNotExist)
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
                    let token = self.token.child_token();
                    let pending_dialog = PendingDialog {
                        token: token.clone(),
                        dialog: dialog.clone(),
                        state_receiver,
                    };

                    let guard = Arc::new(PendingDialogGuard::new(
                        self.invitation.clone(),
                        dialog_id,
                        pending_dialog,
                    ));

                    let accept_timeout = self
                        .config
                        .accept_timeout
                        .as_ref()
                        .and_then(|t| parse_duration(t).ok())
                        .unwrap_or_else(|| Duration::from_secs(60));

                    let token_ref = token.clone();
                    let guard_ref = guard.clone();
                    crate::spawn(async move {
                        select! {
                            _ = token_ref.cancelled() => {}
                            _ = tokio::time::sleep(accept_timeout) => {}
                        }
                        guard_ref.drop_async().await;
                    });

                    let mut dialog_ref = dialog.clone();
                    let token_ref = token.clone();
                    let routing_state = self.routing_state.clone();
                    let dialog_for_reject = dialog.clone();
                    let guard_ref = guard.clone();
                    crate::spawn(async move {
                        let invite_loop = async {
                            match invitation_handler
                                .on_invite(
                                    dialog_id_str.clone(),
                                    token.clone(),
                                    dialog.clone(),
                                    routing_state,
                                )
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    // Webhook failed, reject the call immediately
                                    info!(id = dialog_id_str, "error handling invite: {:?}", e);
                                    let reason = format!("Failed to process invite: {}", e);
                                    if let Err(reject_err) = dialog_for_reject.reject(
                                        Some(rsip::StatusCode::ServiceUnavailable),
                                        Some(reason),
                                    ) {
                                        info!(
                                            id = dialog_id_str,
                                            "error rejecting call: {:?}", reject_err
                                        );
                                    }
                                    // Cancel token to stop dialog handling
                                    token.cancel();
                                    guard_ref.drop_async().await;
                                }
                            }
                        };
                        select! {
                            _ = token_ref.cancelled() => {}
                            _ = async {
                                let (_,_ ) = tokio::join!(dialog_ref.handle(&mut tx), invite_loop);
                             } => {}
                        }
                    });
                }
                rsip::Method::Options => {
                    info!(?key, "ignoring out-of-dialog OPTIONS request");
                    continue;
                }
                _ => {
                    info!(?key, "received request: {:?}", tx.original.method);
                    match tx.reply(rsip::StatusCode::OK).await {
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

    pub async fn graceful_stop(&self) -> Result<()> {
        if self.shutting_down.swap(true, Ordering::Relaxed) {
            return Ok(());
        }

        info!("graceful stopping, marking as shutting down");
        let timeout = self
            .config
            .graceful_shutdown
            .map(|_| Duration::from_secs(10));

        self.stop_registration(timeout).await?;
        self.token.cancel();
        Ok(())
    }

    pub async fn start_registration(&self) -> Result<usize> {
        let mut count = 0;
        if let Some(register_users) = &self.config.register_users {
            for option in register_users.iter() {
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
        let callee_uri = callee
            .strip_prefix("sip:")
            .or_else(|| callee.strip_prefix("sips:"))
            .unwrap_or(callee);
        let callee_uri = if !callee_uri.starts_with("sip:") && !callee_uri.starts_with("sips:") {
            format!("sip:{}", callee_uri)
        } else {
            callee_uri.to_string()
        };

        let parsed_callee = match rsip::Uri::try_from(callee_uri.as_str()) {
            Ok(uri) => uri,
            Err(e) => {
                warn!("failed to parse callee URI: {} {:?}", callee, e);
                return None;
            }
        };

        let callee_host = match &parsed_callee.host_with_port.host {
            rsip::Host::Domain(domain) => domain.to_string(),
            rsip::Host::IpAddr(ip) => return self.find_credentials_by_ip(ip),
        };

        // Look through registered users to find one matching this domain
        if let Some(register_users) = &self.config.register_users {
            for option in register_users.iter() {
                let mut server = option.server.clone();
                if !server.starts_with("sip:") && !server.starts_with("sips:") {
                    server = format!("sip:{}", server);
                }

                let parsed_server = match rsip::Uri::try_from(server.as_str()) {
                    Ok(uri) => uri,
                    Err(e) => {
                        warn!("failed to parse server URI: {} {:?}", option.server, e);
                        continue;
                    }
                };

                let server_host = match &parsed_server.host_with_port.host {
                    rsip::Host::Domain(domain) => domain.to_string(),
                    rsip::Host::IpAddr(ip) => {
                        // Compare IP addresses
                        if let rsip::Host::IpAddr(callee_ip) = &parsed_callee.host_with_port.host {
                            if ip == callee_ip {
                                if let Some(cred) = &option.credential {
                                    info!(
                                        callee,
                                        username = cred.username,
                                        server = option.server,
                                        "Auto-injecting credentials from registered user for outbound call (IP match)"
                                    );
                                    return Some(cred.clone());
                                }
                            }
                        }
                        continue;
                    }
                };

                if server_host == callee_host {
                    if let Some(cred) = &option.credential {
                        info!(
                            callee,
                            username = cred.username,
                            server = option.server,
                            "Auto-injecting credentials from registered user for outbound call"
                        );
                        return Some(cred.clone());
                    }
                }
            }
        }

        None
    }

    /// Helper function to find credentials by IP address
    fn find_credentials_by_ip(
        &self,
        callee_ip: &std::net::IpAddr,
    ) -> Option<crate::useragent::registration::UserCredential> {
        if let Some(register_users) = &self.config.register_users {
            for option in register_users.iter() {
                let mut server = option.server.clone();
                if !server.starts_with("sip:") && !server.starts_with("sips:") {
                    server = format!("sip:{}", server);
                }

                if let Ok(parsed_server) = rsip::Uri::try_from(server.as_str()) {
                    if let rsip::Host::IpAddr(server_ip) = &parsed_server.host_with_port.host {
                        if server_ip == callee_ip {
                            if let Some(cred) = &option.credential {
                                info!(
                                    callee_ip = %callee_ip,
                                    username = cred.username,
                                    server = option.server,
                                    "Auto-injecting credentials from registered user for outbound call (IP match)"
                                );
                                return Some(cred.clone());
                            }
                        }
                    }
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
        let mut server = option.server.clone();
        if !server.starts_with("sip:") && !server.starts_with("sips:") {
            server = format!("sip:{}", server);
        }
        let sip_server = match rsip::Uri::try_from(server) {
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

impl Drop for AppStateInner {
    fn drop(&mut self) {
        self.stop();
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

    pub async fn build(self) -> Result<AppState> {
        let config: Arc<Config> = Arc::new(self.config.unwrap_or_default());
        let token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let _ = set_cache_dir(&config.media_cache_path);
        let local_ip = if !config.addr.is_empty() {
            std::net::IpAddr::from_str(config.addr.as_str())?
        } else {
            crate::net_tool::get_first_non_loopback_interface()?
        };
        let transport_layer = rsipstack::transport::TransportLayer::new(token.clone());
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

        let udp_inner = rsipstack::transport::udp::UdpInner {
            conn: tokio_socket,
            addr: rsipstack::transport::SipAddr {
                r#type: Some(rsip::transport::Transport::Udp),
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
            let tls_sip_addr = rsipstack::transport::SipAddr {
                r#type: Some(rsip::transport::Transport::Tls),
                addr: tls_addr.into(),
            };
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
                tls_sip_addr,
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

        if config.auto_learn_public_address.unwrap_or_default() {
            let inspector = LearningMessageInspector::new(bind_addr.into(), self.message_inspector);
            learned_public_address = inspector.shared_public_address();
            endpoint_builder = endpoint_builder.with_inspector(Box::new(inspector));
        } else if let Some(inspector) = self.message_inspector {
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
            config,
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
            learned_public_address,
            active_calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
            total_calls: AtomicU64::new(0),
            total_failed_calls: AtomicU64::new(0),
            uptime: Local::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
        });

        Ok(app_state)
    }
}
