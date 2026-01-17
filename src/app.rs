use crate::{
    call::{ActiveCallRef, sip::Invitation},
    callrecord::{
        CallRecordFormatter, CallRecordManagerBuilder, CallRecordSender, DefaultCallRecordFormatter,
    },
    config::Config,
    locator::RewriteTargetLocator,
    useragent::{
        RegisterOption,
        invitation::FnCreateInvitationHandler,
        invitation::{PendingDialog, PendingDialogGuard, default_create_invite_handler},
        registration::RegistrationHandle,
    },
};

use crate::media::{cache::set_cache_dir, engine::StreamEngine};
use anyhow::Result;
use chrono::{DateTime, Utc};
use humantime::parse_duration;
use rsip::prelude::HeadersExt;
use rsipstack::transaction::{
    Endpoint, TransactionReceiver,
    endpoint::{TargetLocator, TransportEventInspector},
};
use rsipstack::{dialog::dialog_layer::DialogLayer, transaction::endpoint::MessageInspector};
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::{collections::HashMap, net::SocketAddr};
use std::{path::Path, sync::atomic::AtomicU64};
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
    pub registration_handles: Mutex<HashMap<String, RegistrationHandle>>,
    pub alive_users: Arc<RwLock<HashSet<String>>>,
    pub dialog_layer: Arc<DialogLayer>,
    pub create_invitation_handler: Option<FnCreateInvitationHandler>,
    pub invitation: Invitation,
    pub routing_state: Arc<crate::call::RoutingState>,
    pub pending_playbooks: Arc<Mutex<HashMap<String, String>>>,

    pub active_calls: Arc<std::sync::Mutex<HashMap<String, ActiveCallRef>>>,
    pub total_calls: AtomicU64,
    pub total_failed_calls: AtomicU64,
}

pub type AppState = Arc<AppStateInner>;

pub struct AppStateBuilder {
    pub config: Option<Config>,
    pub stream_engine: Option<Arc<StreamEngine>>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub callrecord_formatter: Option<Arc<dyn CallRecordFormatter>>,
    pub cancel_token: Option<CancellationToken>,
    pub create_invitation_handler: Option<FnCreateInvitationHandler>,
    pub config_loaded_at: Option<DateTime<Utc>>,
    pub config_path: Option<String>,

    pub message_inspector: Option<Box<dyn MessageInspector>>,
    pub target_locator: Option<Box<dyn TargetLocator>>,
    pub transport_inspector: Option<Box<dyn TransportEventInspector>>,
}

impl AppStateInner {
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
            .map(|_| Duration::from_secs(50));

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
                match dialog_layer.match_dialog(&tx.original) {
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
                    let contact = dialog_layer
                        .endpoint
                        .get_addrs()
                        .first()
                        .map(|addr| rsip::Uri {
                            scheme: Some(rsip::Scheme::Sip),
                            auth: None,
                            host_with_port: addr.addr.clone(),
                            params: vec![],
                            headers: vec![],
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
                    crate::spawn(async move {
                        let invite_loop = async {
                            match invitation_handler
                                .on_invite(
                                    dialog_id_str.clone(),
                                    token,
                                    dialog.clone(),
                                    routing_state,
                                )
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!(id = dialog_id_str, "error handling invite: {:?}", e);
                                    guard.drop_async().await;
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
        info!("stopping");
        self.token.cancel();
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

    pub async fn stop_registration(&self, wait_for_clear: Option<Duration>) -> Result<()> {
        for (_, handle) in self.registration_handles.lock().await.iter_mut() {
            handle.stop();
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
        let handle = RegistrationHandle {
            inner: Arc::new(crate::useragent::registration::RegistrationHandleInner {
                endpoint_inner: self.endpoint.inner.clone(),
                option,
                cancel_token,
                start_time: Mutex::new(std::time::Instant::now()),
                last_update: Mutex::new(std::time::Instant::now()),
                last_response: Mutex::new(None),
            }),
        };
        self.registration_handles
            .lock()
            .await
            .insert(user.clone(), handle.clone());
        let alive_users = self.alive_users.clone();

        crate::spawn(async move {
            *handle.inner.start_time.lock().await = std::time::Instant::now();

            select! {
                _ = handle.inner.cancel_token.cancelled() => {
                }
                _ = async {
                    loop {
                        let user = handle.inner.option.aor();
                        alive_users.write().unwrap().remove(&user);
                        let refresh_time = match handle.do_register(&sip_server, None).await {
                            Ok(expires) => {
                                info!(
                                    user = handle.inner.option.aor(),
                                    expires = expires,
                                    alive_users = alive_users.read().unwrap().len(),
                                    "registration refreshed",
                                );
                                alive_users.write().unwrap().insert(user);
                                expires * 3 / 4 // 75% of expiration time
                            }
                            Err(e) => {
                                warn!(
                                    user = handle.inner.option.aor(),
                                    alive_users = alive_users.read().unwrap().len(),
                                    "registration failed: {:?}", e);
                                60
                            }
                        };
                        tokio::time::sleep(Duration::from_secs(refresh_time as u64)).await;
                    }
                } => {}
            }
            handle.do_register(&sip_server, Some(0)).await.ok();
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
            config_loaded_at: None,
            config_path: None,
            message_inspector: None,
            target_locator: None,
            transport_inspector: None,
        }
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        if self.config_loaded_at.is_none() {
            self.config_loaded_at = Some(Utc::now());
        }
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

    pub fn with_config_metadata(mut self, path: Option<String>, loaded_at: DateTime<Utc>) -> Self {
        self.config_path = path;
        self.config_loaded_at = Some(loaded_at);
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

        let udp_conn = rsipstack::transport::udp::UdpConnection::create_connection(
            local_addr,
            None,
            Some(token.child_token()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Create useragent UDP connection: {} {}", local_addr, e))?;

        transport_layer.add_transport(udp_conn.into());
        info!("start useragent, addr: {}", local_addr);

        let endpoint_option = rsipstack::transaction::endpoint::EndpointOption::default();
        let mut endpoint_builder = rsipstack::EndpointBuilder::new();
        if let Some(ref user_agent) = config.useragent {
            endpoint_builder.with_user_agent(user_agent.as_str());
        }

        let mut endpoint_builder = endpoint_builder
            .with_cancel_token(token.child_token())
            .with_transport_layer(transport_layer)
            .with_option(endpoint_option);

        if let Some(inspector) = self.message_inspector {
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
            active_calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
            total_calls: AtomicU64::new(0),
            total_failed_calls: AtomicU64::new(0),
        });

        Ok(app_state)
    }
}
