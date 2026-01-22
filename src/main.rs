use active_call::media::engine::StreamEngine;
use anyhow::Result;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use dotenvy::dotenv;
use reqwest::StatusCode;
use std::sync::Arc;
use tokio::signal;
use tower_http::services::ServeDir;
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::LocalTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use active_call::app::AppStateBuilder;
use active_call::config::{Cli, Config};
use uuid::Uuid;

pub async fn index() -> impl IntoResponse {
    match std::fs::read_to_string("static/index.html") {
        Ok(content) => (StatusCode::OK, [("content-type", "text/html")], content).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Index not found").into_response(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    dotenv().ok();

    let cli = Cli::parse();

    // Handle model download if requested
    #[cfg(feature = "offline")]
    if let Some(model_type) = &cli.download_models {
        use active_call::offline::{ModelDownloader, ModelType};
        use std::path::PathBuf;

        let models_dir = PathBuf::from(&cli.models_dir);
        let downloader = ModelDownloader::new()?;

        let model = ModelType::from_str(model_type).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown model type: {}. Use: sensevoice, supertonic, or all",
                model_type
            )
        })?;

        downloader.download(model, &models_dir)?;
        println!("✓ Models downloaded to: {}", models_dir.display());

        if cli.exit_after_download {
            return Ok(());
        }
    }

    // Initialize offline models if feature is enabled
    #[cfg(feature = "offline")]
    {
        use active_call::offline::{OfflineConfig, init_offline_models};
        use std::path::PathBuf;

        let offline_config =
            OfflineConfig::new(PathBuf::from(&cli.models_dir), num_cpus::get().min(4));

        // Only initialize if models directory exists
        if offline_config.models_dir.exists() {
            init_offline_models(offline_config)?;
            println!("Offline models initialized from: {}", cli.models_dir);
        } else {
            println!(
                "Models directory not found: {}. Offline features will not be available. Run with --download-models to download.",
                cli.models_dir
            );
        }
    }

    let (mut config, config_path) = if let Some(path) = cli.conf {
        let config = Config::load(&path).unwrap_or_else(|e| {
            println!("Failed to load config from {}: {}, using defaults", path, e);
            Config::default()
        });
        (config, Some(path))
    } else {
        (Config::default(), None)
    };

    if let Some(http) = cli.http {
        config.http_addr = http;
    }

    if let Some(sip) = cli.sip {
        if let Ok(port) = sip.parse::<u16>() {
            config.udp_port = port;
        } else if let Ok(socket_addr) = sip.parse::<std::net::SocketAddr>() {
            config.addr = socket_addr.ip().to_string();
            config.udp_port = socket_addr.port();
        } else {
            config.addr = sip;
        }
    }

    // Auto-configure handler from CLI parameter
    if let Some(handler_str) = &cli.handler {
        use active_call::config::InviteHandlerConfig;

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

    if let Some(external_ip) = cli.external_ip {
        config.external_ip = Some(external_ip);
    }

    if let Some(codecs) = cli.codecs {
        config.codecs = Some(codecs);
    }

    let mut env_filter = EnvFilter::from_default_env();
    if let Some(Ok(level)) = config
        .log_level
        .as_ref()
        .map(|level| level.parse::<LevelFilter>())
    {
        env_filter = env_filter.add_directive(level.into());
    }
    env_filter = env_filter.add_directive("ort=warn".parse()?);
    let mut file_layer = None;
    let mut guard_holder = None;
    let mut fmt_layer = None;
    if let Some(ref log_file) = config.log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)
            .expect("Failed to open log file");
        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        guard_holder = Some(guard);
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

    let _ = guard_holder; // keep the guard alive

    info!("Starting active-call service...");

    let stream_engine = Arc::new(StreamEngine::default());

    let app_state = AppStateBuilder::new()
        .with_config(config.clone())
        .with_stream_engine(stream_engine)
        .with_config_metadata(config_path)
        .build()
        .await?;

    info!("AppState started");

    // Handle CLI direct call if requested
    if let Some(callee) = cli.call {
        let app_state_clone = app_state.clone();
        let playbook = if let Some(h) = &cli.handler {
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

            if let Some(playbook) = playbook {
                app_state_clone
                    .pending_playbooks
                    .lock()
                    .await
                    .insert(session_id.clone(), playbook);
            }

            let (command_sender, command_receiver) = tokio::sync::mpsc::unbounded_channel();
            let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel();
            let (_audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();

            use active_call::CallOption;
            use active_call::call::{ActiveCallType, Command};

            let invite_cmd = Command::Invite {
                option: CallOption {
                    callee: Some(callee),
                    ..Default::default()
                },
            };

            let _ = command_sender.send(invite_cmd);

            active_call::handler::handler::call_handler_core(
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
            )
            .await;
        });
    }

    let http_addr = config.http_addr.clone();
    let listener = tokio::net::TcpListener::bind(&http_addr).await?;
    info!("listening on http://{}", http_addr);

    let app = active_call::handler::call_router()
        .merge(active_call::handler::playbook_router())
        .merge(active_call::handler::iceservers_router())
        .route("/", get(index))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(app_state.clone());

    tokio::select! {
        result = axum::serve(listener, app) => {
            if let Err(e) = result {
                warn!("axum serve error: {:?}", e);
            }
        }
        res = app_state.serve() => {
            if let Err(e) = res {
                warn!("AppState server error: {}", e);
            }
        }
        _ = signal::ctrl_c() => {
            info!("Shutdown signal received");
        }
    }
    info!("Shutting down...");
    Ok(())
}
