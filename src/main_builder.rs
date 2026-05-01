use crate::media::engine::StreamEngine;
use anyhow::Result;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use dotenvy::dotenv;
use futures::{FutureExt, future};
use reqwest::StatusCode;
use std::sync::Arc;
use tokio::signal;
use tower_http::services::ServeDir;
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::LocalTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::app::{AppStateBuilder, AppStateInner};
use crate::config::{Cli, Config};
use uuid::Uuid;

pub struct MainBuilder {
    pub cli: Cli,
    pub config: Config,
    /// Default active-call routes
    pub router: Router<Arc<AppStateInner>>,
    /// If None - default will be used
    pub stream_engine: Option<Arc<StreamEngine>>,
    /// Will use Handle::current, otherwise creates this default runtime
    pub runtime: Option<tokio::runtime::Runtime>,
    /// To keep the logging guard alive
    pub guard_holder: Option<WorkerGuard>,
}

impl MainBuilder {
    pub fn main() -> Result<()> {
        Self::default()?.run()
    }

    pub fn default() -> Result<Self> {
        let cli = Cli::parse();
        Self::from_cli(cli)
    }

    pub fn from_cli(cli: Cli) -> Result<Self> {
        let config = if let Some(path) = &cli.conf {
            Config::load(&path).unwrap_or_else(|e| {
                println!("Failed to load config from {}: {}, using defaults", path, e);
                Config::default()
            })
        } else {
            Config::default()
        };
        Ok(Self::new(cli, config))
    }

    pub fn new(cli: Cli, config: Config) -> Self {
        let config = Self::apply_cli_overrides(&cli, config);
        MainBuilder {
            cli,
            config,
            runtime: None,
            stream_engine: None,
            router: Self::default_router(),
            guard_holder: None,
        }
    }

    fn apply_cli_overrides(cli: &Cli, mut config: Config) -> Config {
        if let Some(ref http) = cli.http {
            config.http_addr = http.clone();
        }

        if let Some(ref sip) = cli.sip {
            if let Ok(port) = sip.parse::<u16>() {
                config.udp_port = port;
            } else if let Ok(socket_addr) = sip.parse::<std::net::SocketAddr>() {
                config.addr = socket_addr.ip().to_string();
                config.udp_port = socket_addr.port();
            } else {
                config.addr = sip.clone();
            }
        }

        // Auto-configure handler from CLI parameter
        if let Some(handler_str) = &cli.handler {
            use crate::config::InviteHandlerConfig;

            if handler_str.starts_with("http://") || handler_str.starts_with("https://") {
                // Webhook handler
                config.handler = Some(InviteHandlerConfig::Webhook {
                    url: Some(handler_str.clone()),
                    urls: None,
                    method: None,
                    headers: None,
                });
                info!("CLI handler configured as webhook: {}", handler_str);
            } else if handler_str.ends_with(".md") {
                // Playbook handler with default playbook
                config.handler = Some(InviteHandlerConfig::Playbook {
                    rules: None,
                    default: Some(handler_str.clone()),
                });
                info!(
                    "CLI handler configured as playbook default: {}",
                    handler_str
                );
            } else {
                warn!(
                    "Invalid handler format: {}. Should be http(s):// URL or .md file",
                    handler_str
                );
            }
        }

        if let Some(ref external_ip) = cli.external_ip {
            config.external_ip = Some(external_ip.clone());
        }

        if let Some(ref codecs) = cli.codecs {
            config.codecs = Some(codecs.clone());
        }

        config
    }

    pub fn run(&mut self) -> Result<()> {
        Self::init();
        #[cfg(feature = "offline")]
        self.handle_offline()?;
        self.setup_logging()?;
        info!("Starting active-call service...");

        if let Ok(current) = tokio::runtime::Handle::try_current() {
            current.block_on(self.run_async())
        } else if let Some(ref rt) = self.runtime {
            rt.block_on(self.run_async())
        } else {
            self.runtime = Some(tokio::runtime::Runtime::new()?);
            self.runtime.as_ref().unwrap().block_on(self.run_async())
        }
    }

    async fn run_async(&self) -> Result<()> {
        let app_state = self.build_app_state().await?;
        self.handle_cli_direct_call(app_state.clone()).await;
        let listener = self.build_tcp_listener()?;
        let router = self.router.clone().with_state(app_state.clone());
        self.serve(router, app_state, listener).await
    }

    fn init() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .expect("Failed to install rustls crypto provider");
        dotenv().ok();
    }

    #[cfg(feature = "offline")]
    fn handle_offline(&self) -> Result<()> {
        use crate::offline::{ModelDownloader, ModelType, OfflineConfig, init_offline_models};
        use std::path::PathBuf;

        // Handle model download if requested
        if let Some(model_type) = &self.cli.download_models {
            let models_dir = PathBuf::from(&self.cli.models_dir);
            let downloader = ModelDownloader::new()?;

            let model = ModelType::from_str(model_type).ok_or_else(|| {
                anyhow::anyhow!(
                    "Unknown model type: {}. Use: sensevoice, supertonic, or all",
                    model_type
                )
            })?;

            downloader.download(model, &models_dir)?;
            println!("✓ Models downloaded to: {}", models_dir.display());

            if self.cli.exit_after_download {
                return Ok(());
            }
        }

        // Initialize offline models
        let offline_config =
            OfflineConfig::new(PathBuf::from(&self.cli.models_dir), num_cpus::get().min(4));

        // Only initialize if models directory exists
        if offline_config.models_dir.exists() {
            init_offline_models(offline_config)?;
            println!("Offline models initialized from: {}", self.cli.models_dir);
        } else {
            println!(
                "Models directory not found: {}. Offline features will not be available. Run with --download-models to download.",
                self.cli.models_dir
            );
        }

        Ok(())
    }

    fn setup_logging(&mut self) -> Result<()> {
        let mut env_filter = EnvFilter::from_default_env();
        if let Some(Ok(level)) = self
            .config
            .log_level
            .as_ref()
            .map(|level| level.parse::<LevelFilter>())
        {
            env_filter = env_filter.add_directive(level.into());
        }
        env_filter = env_filter.add_directive("ort=warn".parse()?);
        let mut file_layer = None;
        let mut fmt_layer = None;
        if let Some(ref log_file) = self.config.log_file {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_file)
                .expect("Failed to open log file");
            let (non_blocking, guard) = tracing_appender::non_blocking(file);
            self.guard_holder = Some(guard);
            file_layer = Some(
                tracing_subscriber::fmt::layer()
                    .with_timer(LocalTime::rfc_3339())
                    .with_ansi(false)
                    .with_writer(non_blocking),
            );
        } else {
            fmt_layer = Some(tracing_subscriber::fmt::layer().with_timer(LocalTime::rfc_3339()));
        }

        if let Some(file_layer) = file_layer {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(file_layer)
                .try_init()?;
        } else if let Some(fmt_layer) = fmt_layer {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .try_init()?;
        }

        Ok(())
    }

    async fn build_app_state(&self) -> Result<Arc<AppStateInner>> {
        let stream_engine = self
            .stream_engine
            .clone()
            .unwrap_or_else(|| Arc::new(StreamEngine::default()));

        let result = AppStateBuilder::new()
            .with_config(self.config.clone())
            .with_stream_engine(stream_engine)
            .with_config_metadata(self.cli.conf.clone())
            .build()
            .await?;
        info!("AppState started");
        Ok(result)
    }

    async fn handle_cli_direct_call(&self, app_state: Arc<AppStateInner>) {
        if let Some(ref callee) = self.cli.call {
            let callee = callee.clone();
            let app_state_clone = app_state.clone();
            let playbook = if let Some(h) = &self.cli.handler {
                if h.ends_with(".md") {
                    Some(h.clone())
                } else {
                    None
                }
            } else {
                None
            };

            tokio::spawn(async move {
                // Wait a bit for the SIP stack to initialize
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                let session_id = format!("c.{}", Uuid::new_v4());
                info!(session_id, "Starting CLI outgoing call to: {}", callee);

                let (command_sender, command_receiver) = tokio::sync::mpsc::unbounded_channel();
                let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel();
                let (_audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();

                use crate::CallOption;
                use crate::call::{ActiveCallType, Command};

                let invite_cmd = Command::Invite {
                    option: CallOption {
                        callee: Some(callee.clone()),
                        ..Default::default()
                    },
                };

                let _ = command_sender.send(invite_cmd);

                crate::handler::handler::call_handler_core(
                    ActiveCallType::Sip,
                    session_id,
                    app_state_clone,
                    tokio_util::sync::CancellationToken::new(),
                    audio_rx,
                    None,
                    false,
                    0,
                    command_receiver,
                    event_sender,
                    None,     // extras
                    playbook, // playbook_name — passed directly
                )
                .await;
            });
        }
    }

    fn build_tcp_listener(&self) -> Result<tokio::net::TcpListener> {
        let http_addr = self.config.http_addr.clone();

        // Create TCP listener with SO_REUSEPORT for graceful restarts
        let addr: std::net::SocketAddr = http_addr.parse()?;
        // Create socket manually to set SO_REUSEPORT before bind
        let std_listener = {
            use socket2::{Domain, Protocol, Socket, Type};

            let domain = if addr.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
            socket.set_reuse_address(true)?;
            #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
            socket.set_reuse_port(true)?;
            socket.bind(&addr.into())?;
            socket.listen(1024)?;
            socket.set_nonblocking(true)?;
            std::net::TcpListener::from(socket)
        };

        let listener = tokio::net::TcpListener::from_std(std_listener)?;
        info!("listening on http://{} (SO_REUSEPORT enabled)", http_addr);
        Ok(listener)
    }

    fn default_router() -> Router<Arc<AppStateInner>> {
        let router = crate::handler::call_router()
            .merge(crate::handler::playbook_router())
            .merge(crate::handler::iceservers_router())
            .route("/", get(index))
            .nest_service("/static", ServeDir::new("static"));
        router
    }

    async fn serve(
        &self,
        router: Router,
        app_state: Arc<AppStateInner>,
        listener: tokio::net::TcpListener,
    ) -> Result<()> {
        let app_state_clone = app_state.clone();
        let graceful_shutdown = self.config.graceful_shutdown.unwrap_or_default();

        let axum_serving = axum::serve(listener, router).into_future();
        let app_state_serving = app_state_clone.serve();
        let mut canceled = false;
        let cancel_timeout = future::pending().boxed();
        let shutdown_task = future::pending::<anyhow::Result<()>>().boxed();
        let shutdown_signal = shutdown_signal().boxed();

        tokio::pin!(axum_serving);
        tokio::pin!(app_state_serving);
        tokio::pin!(cancel_timeout);
        tokio::pin!(shutdown_task);
        tokio::pin!(shutdown_signal);

        loop {
            tokio::select! {
                result = &mut axum_serving => {
                    if let Err(e) = result {
                        warn!("axum serve error: {:?}", e);
                    }
                    break;
                }
                res = &mut app_state_serving => {
                    if let Err(e) = res {
                        warn!("AppState server error: {}", e);
                    }
                    break;
                }
                res = &mut shutdown_task, if canceled => {
                    if let Err(e) = res {
                        warn!("Graceful AppState shutdown failed: {}", e);
                        app_state.stop();
                    }
                }
                signal = &mut shutdown_signal, if !canceled => {
                    match signal {
                        Ok(ShutdownSignal::CtrlC) => info!("SIGINT (Ctrl-C) received"),
                        Ok(ShutdownSignal::SigTerm) => info!("SIGTERM received"),
                        Err(e) => {
                            warn!("Shutdown signal handler failed: {}", e);
                            break;
                        }
                    }
                    if graceful_shutdown {
                        let app_state = app_state.clone();
                        shutdown_task.set(async move { app_state.graceful_stop().await }.boxed());
                        *cancel_timeout = tokio::time::sleep(tokio::time::Duration::from_secs(30)).boxed();
                        canceled = true;
                    } else {
                        break;
                    }
                }
                _ = &mut cancel_timeout => {
                    warn!("Shutdown timeout reached, forcing exit");
                    break;
                }
            }
        }
        Ok(())
    }
}

pub async fn index() -> impl IntoResponse {
    match std::fs::read_to_string("static/index.html") {
        Ok(content) => (StatusCode::OK, [("content-type", "text/html")], content).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Index not found").into_response(),
    }
}

enum ShutdownSignal {
    CtrlC,
    SigTerm,
}

async fn shutdown_signal() -> Result<ShutdownSignal> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = signal::ctrl_c() => {
                result?;
                Ok(ShutdownSignal::CtrlC)
            }
            _ = sigterm.recv() => Ok(ShutdownSignal::SigTerm),
        }
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c().await?;
        Ok(ShutdownSignal::CtrlC)
    }
}
