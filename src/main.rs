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

pub async fn index() -> impl IntoResponse {
    match std::fs::read_to_string("static/index.html") {
        Ok(content) => (StatusCode::OK, [("content-type", "text/html")], content).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Index not found").into_response(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    dotenv().ok();

    let cli = Cli::parse();
    let (mut config, config_path) = if let Some(path) = cli.conf {
        let config = Config::load(&path).unwrap_or_else(|e| {
            warn!("Failed to load config from {}: {}, using defaults", path, e);
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

    let mut env_filter = EnvFilter::from_default_env();
    if let Some(Ok(level)) = config
        .log_level
        .as_ref()
        .map(|level| level.parse::<LevelFilter>())
    {
        env_filter = env_filter.add_directive(level.into());
    }

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
        .with_config_metadata(config_path, chrono::Utc::now())
        .build()
        .await?;

    info!("AppState started");

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
