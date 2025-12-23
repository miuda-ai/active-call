use anyhow::Result;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use dotenv::dotenv;
use reqwest::StatusCode;
use std::sync::Arc;
use tokio::signal;
use tower_http::services::ServeDir;
use tracing::{Level, info, warn};
use tracing_subscriber::FmtSubscriber;
use voice_engine::media::engine::StreamEngine;

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

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    info!("Starting active-call service...");

    let stream_engine = Arc::new(StreamEngine::new());

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
