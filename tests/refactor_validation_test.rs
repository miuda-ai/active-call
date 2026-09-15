use active_call::{
    app::AppStateBuilder,
    config::{Config, InviteHandlerConfig},
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{Level, info};

#[derive(Debug, Deserialize, Serialize)]
struct WebhookPayload {
    #[serde(rename = "dialogId")]
    dialog_id: String,
    event: String,
}

fn create_test_config(sip_port: u16, http_port: u16, webhook_port: u16) -> Config {
    Config {
        http_addr: format!("127.0.0.1:{}", http_port),
        addr: "127.0.0.1".to_string(),
        udp_port: sip_port,
        log_level: Some("debug".to_string()),
        handler: Some(InviteHandlerConfig::Webhook {
            url: Some(format!("http://127.0.0.1:{}/mock-handler", webhook_port)),
            method: Some("POST".to_string()),
            headers: None,
            urls: None,
        }),
        accept_timeout: Some("5s".to_string()),
        rtp_start_port: Some(40000 + (sip_port % 1000) * 20),
        rtp_end_port: Some(40020 + (sip_port % 1000) * 20),
        media_cache_path: "./target/tmp_media_test".to_string(),
        ..Default::default()
    }
}

async fn start_webhook_server(port: u16, tx: mpsc::Sender<String>) {
    use warp::Filter;
    let route = warp::post()
        .and(warp::path("mock-handler"))
        .and(warp::body::json())
        .map(move |payload: WebhookPayload| {
            if payload.event == "invite" {
                let _ = tx.try_send(payload.dialog_id);
            }
            warp::reply::json(&serde_json::json!({"status": "ok"}))
        });

    tokio::spawn(warp::serve(route).run(([127, 0, 0, 1], port)));
}

async fn has_sipbot() -> bool {
    tokio::process::Command::new("sipbot")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn test_call_lifecycle_play_and_hangup() {
    if !has_sipbot().await {
        info!("sipbot not found, skipping test");
        return;
    }
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_test_writer()
        .try_init()
        .ok();

    let sip_port = 35070;
    let http_port = 9070;
    let webhook_port = 9970;
    let config = create_test_config(sip_port, http_port, webhook_port);

    let (tx, mut rx) = mpsc::channel(1);
    start_webhook_server(webhook_port, tx).await;

    let builder = AppStateBuilder::new().with_config(config);
    let app = builder.build().await.expect("Failed to build app state");
    let app_for_serve = app.clone();
    tokio::spawn(app_for_serve.serve());

    let router = active_call::handler::handler::call_router().with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", http_port))
        .await
        .unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start sipbot call
    let mut child = tokio::process::Command::new("sipbot")
        .args(&[
            "call",
            "--target",
            &format!("sip:100@127.0.0.1:{}", sip_port),
            "--external",
            "127.0.0.1",
            "--hangup",
            "30",
        ])
        .spawn()
        .expect("Failed to spawn sipbot");

    let dialog_id = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("Timed out waiting for invite")
        .expect("Channel closed");

    let ws_url = format!("ws://127.0.0.1:{}/call/sip?id={}", http_port, dialog_id);
    let (ws_stream, _) = connect_async(ws_url).await.expect("WS connect failed");
    let (mut ws_write, mut ws_read) = ws_stream.split();

    // 1. Accept
    ws_write
        .send(Message::Text(
            serde_json::json!({
                "command": "accept",
                "option": {}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // Wait for answer event
    let mut answered = false;
    let mut track_started = false;

    let timeout = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        info!("WS Event Received: {}", text);
                        if text.contains("\"event\":\"answer\"") {
                            answered = true;
                        }
                        if text.contains("\"event\":\"trackStart\"") {
                            track_started = true;
                        }
                        if answered && track_started {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => panic!("WS read error: {}", e),
                    None => break,
                }
            }
            _ = &mut timeout => {
                break;
            }
        }
    }
    assert!(answered, "Failed to receive answer event");
    assert!(track_started, "Failed to receive trackStart event");

    // 2. Play a file
    // 确保使用一个存在的小文件，fixtures/sample.wav 通常是存在的
    let test_file = "fixtures/sample.wav";
    ws_write
        .send(Message::Text(
            serde_json::json!({
                "command": "play",
                "url": test_file,
                "play_id": "test_play_1"
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // 3. Wait for TrackEnd
    let mut track_ended = false;
    let timeout = tokio::time::sleep(Duration::from_secs(15)); // 增加超时到 15s
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        info!("WS Event Received: {}", text);
                        // 注意转换成 camelCase 或保持一致
                        if text.contains("\"event\":\"trackEnd\"") {
                            track_ended = true;
                            // Optionally check if it contains our play_id OR the filename
                            break;
                        }
                    }
                    Some(Ok(_)) => {
                        // ignore ping/pong/binary
                    }
                    _ => {
                        break;
                    }
                }
            }
            _ = &mut timeout => {
                break;
            }
        }
    }
    assert!(track_ended, "Failed to receive trackEnd event for play");

    // 4. Hangup
    ws_write
        .send(Message::Text(
            serde_json::json!({"command": "hangup"}).to_string().into(),
        ))
        .await
        .unwrap();

    // Wait for sipbot to exit
    let _ = child.wait().await;
}

#[tokio::test]
async fn test_app_invite_sipbot_wait() {
    if !has_sipbot().await {
        info!("sipbot not found, skipping test");
        return;
    }
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_test_writer()
        .try_init()
        .ok();

    let sip_port = 35071;
    let http_port = 9071;
    let webhook_port = 9971;
    let bot_sip_port = 35080;

    let config = create_test_config(sip_port, http_port, webhook_port);

    // 启动系统
    let builder = AppStateBuilder::new().with_config(config);
    let app = builder.build().await.expect("Failed to build app state");
    tokio::spawn(app.clone().serve());

    let router = active_call::handler::handler::call_router().with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", http_port))
        .await
        .unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // 启动 sipbot wait (等待呼叫)
    let mut child = tokio::process::Command::new("sipbot")
        .args(&[
            "wait",
            "--addr",
            &format!("127.0.0.1:{}", bot_sip_port),
            "--answer",
            "fixtures/sample.wav",
            "--hangup",
            "5",
        ])
        .spawn()
        .expect("Failed to spawn sipbot wait");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // 通过 WebSocket 建立连接并发送 invite
    let session_id = "test_invite_session";
    let ws_url = format!("ws://127.0.0.1:{}/call/sip?id={}", http_port, session_id);
    let (ws_stream, _) = connect_async(ws_url).await.expect("WS connect failed");
    let (mut ws_write, mut ws_read) = ws_stream.split();

    // 发送 invite 指令去呼叫 sipbot
    ws_write
        .send(Message::Text(
            serde_json::json!({
                "command": "invite",
                "option": {
                    "callee": format!("sip:bot@127.0.0.1:{}", bot_sip_port)
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // 等待 answer 和 trackStart
    let mut answered = false;
    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            msg = ws_read.next() => {
                if let Some(Ok(Message::Text(text))) = msg {
                    info!("WS Event: {}", text);
                    if text.contains("\"event\":\"answer\"") {
                        answered = true;
                        break;
                    }
                } else if msg.is_none() {
                    break;
                }
            }
            _ = &mut timeout => { break; }
        }
    }
    assert!(answered, "App failed to receive answer from sipbot");

    // 挂断
    ws_write
        .send(Message::Text(
            serde_json::json!({"command": "hangup"}).to_string().into(),
        ))
        .await
        .unwrap();

    let _ = child.kill().await;
}
