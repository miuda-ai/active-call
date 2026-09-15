/// Test WebRTC audio reception to diagnose the issue where browser audio is not received
use active_call::{app::AppStateBuilder, config::Config};
use anyhow::Result;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{Level, info, warn};

// Sample WebRTC offer with audio track from a browser
const BROWSER_OFFER: &str = r#"v=0
o=- 3202329840648393745 2 IN IP4 127.0.0.1
s=-
t=0 0
a=group:BUNDLE 0
a=extmap-allow-mixed
a=msid-semantic: WMS test-stream
m=audio 9 UDP/TLS/RTP/SAVPF 111 0 8
c=IN IP4 0.0.0.0
a=rtcp:9 IN IP4 0.0.0.0
a=ice-ufrag:test123
a=ice-pwd:testpassword123456789012
a=ice-options:trickle
a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99
a=setup:actpass
a=mid:0
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time
a=sendrecv
a=msid:test-stream audio-track-1
a=rtcp-mux
a=rtpmap:111 opus/48000/2
a=fmtp:111 minptime=10;useinbandfec=1
a=rtpmap:0 PCMU/8000
a=rtpmap:8 PCMA/8000
a=ssrc:12345 cname:test-cname
a=ssrc:12345 msid:test-stream audio-track-1
"#;

fn create_test_config(http_port: u16, sip_port: u16) -> Config {
    Config {
        http_addr: format!("127.0.0.1:{}", http_port),
        addr: "0.0.0.0".to_string(),
        udp_port: sip_port,
        log_level: Some("debug".to_string()),
        http_access_skip_paths: vec![],
        useragent: Some("WebRTC-Audio-Test".to_string()),
        accept_timeout: Some("5s".to_string()),
        rtp_start_port: Some(40000),
        rtp_end_port: Some(40100),
        media_cache_path: "./target/tmp_media_test".to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn test_webrtc_audio_reception_with_hello_playbook() -> Result<()> {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let http_port = portpicker::pick_unused_port().expect("No free port");
    let sip_port = portpicker::pick_unused_port().expect("No free port");

    info!(
        "Starting test with http_port={}, sip_port={}",
        http_port, sip_port
    );

    let config = create_test_config(http_port, sip_port);
    let builder = AppStateBuilder::new().with_config(config.clone());
    let app_state = builder.build().await?;

    // Start HTTP server
    let http_addr = format!("127.0.0.1:{}", http_port);
    let listener = tokio::net::TcpListener::bind(&http_addr).await?;
    info!("Test HTTP server listening on {}", http_addr);

    let app = active_call::handler::call_router()
        .merge(active_call::handler::playbook_router())
        .merge(active_call::handler::iceservers_router())
        .with_state(app_state.clone());

    // Spawn both servers
    let app_state_clone = app_state.clone();
    let _sip_handle = tokio::spawn(async move { app_state_clone.serve().await });

    let server_handle = tokio::spawn(async move { axum::serve(listener, app).await });

    // Give servers time to start
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 1: Load hello.md playbook and create WebRTC call
    let client = reqwest::Client::new();
    let playbook_content = tokio::fs::read_to_string("./config/playbook/hello.md")
        .await
        .expect("Failed to read hello.md playbook");

    info!("Creating WebRTC call with hello.md playbook");

    let create_response = client
        .post(&format!("http://127.0.0.1:{}/api/playbook/run", http_port))
        .json(&json!({
            "content": playbook_content,
            "type": "webrtc"
        }))
        .send()
        .await?;

    assert!(
        create_response.status().is_success(),
        "Failed to create call: {:?}",
        create_response.text().await
    );

    let create_data: serde_json::Value = create_response.json().await?;
    let session_id = create_data["session_id"]
        .as_str()
        .expect("No session_id in response");

    info!("Created session: {}", session_id);

    // Step 2: Connect via WebSocket
    let ws_url = format!("ws://127.0.0.1:{}/call/webrtc?id={}", http_port, session_id);
    info!("Connecting to WebSocket: {}", ws_url);

    let (mut ws_stream, _) = connect_async(&ws_url).await?;
    info!("WebSocket connected");

    // Step 3: Send INVITE with browser offer
    info!("Sending INVITE with browser offer");
    let invite_msg = json!({
        "command": "invite",
        "option": {
            "offer": BROWSER_OFFER
        }
    });

    ws_stream
        .send(Message::Text(invite_msg.to_string().into()))
        .await?;

    // Step 4: Wait for answer
    info!("Waiting for answer from server...");
    let mut got_answer = false;
    let mut got_greeting = false;
    let mut event_count = 0;

    let result = timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    event_count += 1;
                    info!(
                        "Received WebSocket message #{}: {}",
                        event_count,
                        if text.len() > 200 {
                            format!("{}...", &text[..200])
                        } else {
                            text.to_string()
                        }
                    );

                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                        let event_type = data["event"].as_str().unwrap_or("");

                        match event_type {
                            "answer" => {
                                info!("✓ Received answer from server");
                                got_answer = true;

                                // Verify answer contains SDP
                                let sdp = data["sdp"].as_str().expect("No SDP in answer");
                                assert!(!sdp.is_empty(), "SDP is empty");
                                assert!(sdp.contains("m=audio"), "SDP missing audio media line");
                                info!(
                                    "  Answer SDP preview: {}",
                                    &sdp[..std::cmp::min(100, sdp.len())]
                                );
                            }
                            "metrics" => {
                                let key = data["key"].as_str().unwrap_or("");
                                info!("Received metric key: {}", key);
                                if key == "tts_play_id_map" || key.starts_with("completed.tts") {
                                    got_greeting = true;
                                    info!("✓ Received greeting (TTS event: {})", key);
                                }
                            }
                            "asrfinal" => {
                                let text = data["text"].as_str().unwrap_or("");
                                info!("  ASR recognized: '{}'", text);
                            }
                            _ => {
                                // Log other events
                            }
                        }

                        // If we got both answer and greeting, test is successful
                        if got_answer && got_greeting {
                            break;
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    warn!("WebSocket closed by server");
                    break;
                }
                Err(e) => {
                    warn!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    info!("Test completed. Events received: {}", event_count);
    info!("Got answer: {}, Got greeting: {}", got_answer, got_greeting);

    assert!(result.is_ok(), "Test timed out waiting for events");
    assert!(got_answer, "Did not receive answer from server");
    assert!(got_greeting, "Did not receive greeting (TTS) from server");

    info!("✓ Test passed: WebRTC call established and hello.md playbook executed successfully");

    // Cleanup
    drop(ws_stream);
    app_state.stop();
    let _ = timeout(Duration::from_secs(2), server_handle).await;

    Ok(())
}

/// Test that verifies audio track is properly created and events are received
#[tokio::test]
async fn test_webrtc_track_creation() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let http_port = portpicker::pick_unused_port().expect("No free port");
    let sip_port = portpicker::pick_unused_port().expect("No free port");

    info!("Test: Verifying WebRTC track creation");

    let config = create_test_config(http_port, sip_port);
    let builder = AppStateBuilder::new().with_config(config.clone());
    let app_state = builder.build().await?;

    // Start HTTP server
    let http_addr = format!("127.0.0.1:{}", http_port);
    let listener = tokio::net::TcpListener::bind(&http_addr).await?;

    let app = active_call::handler::call_router()
        .merge(active_call::handler::playbook_router())
        .merge(active_call::handler::iceservers_router())
        .with_state(app_state.clone());

    let app_state_clone = app_state.clone();
    let _sip_handle = tokio::spawn(async move { app_state_clone.serve().await });

    let server_handle = tokio::spawn(async move { axum::serve(listener, app).await });

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client = reqwest::Client::new();
    let playbook_content = r#"---
asr:
  provider: "aliyun"
tts:
  provider: "aliyun"
vad:
  provider: "silero"
---
Test playbook for track verification
"#;

    let create_response = client
        .post(&format!("http://127.0.0.1:{}/api/playbook/run", http_port))
        .json(&json!({
            "content": playbook_content,
            "type": "webrtc"
        }))
        .send()
        .await?;

    assert!(create_response.status().is_success());
    let create_data: serde_json::Value = create_response.json().await?;
    let session_id = create_data["session_id"].as_str().unwrap();

    let ws_url = format!("ws://127.0.0.1:{}/call/webrtc?id={}", http_port, session_id);
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    // Send invite
    let invite_msg = json!({
        "command": "invite",
        "option": {
            "offer": BROWSER_OFFER
        }
    });

    ws_stream
        .send(Message::Text(invite_msg.to_string().into()))
        .await?;

    // Wait for answer and verify track info
    let result = timeout(Duration::from_secs(5), async {
        let mut got_answer = false;
        while let Some(msg) = ws_stream.next().await {
            if let Ok(Message::Text(text)) = msg {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                    if data["event"].as_str() == Some("answer") {
                        got_answer = true;
                        let sdp = data["sdp"].as_str().expect("No SDP");

                        // Verify SDP contains necessary elements
                        assert!(sdp.contains("m=audio"), "Missing audio media line");
                        assert!(sdp.contains("a=rtpmap:"), "Missing rtpmap attributes");
                        assert!(sdp.contains("a=sendrecv"), "Missing sendrecv attribute");

                        info!("✓ Track configuration verified in SDP");
                        break;
                    }
                }
            }
        }
        assert!(got_answer, "No answer received");
    })
    .await;

    assert!(result.is_ok(), "Track verification timed out");
    info!("✓ WebRTC track created successfully");

    // Cleanup
    app_state.stop();
    let _ = timeout(Duration::from_secs(2), server_handle).await;

    Ok(())
}
