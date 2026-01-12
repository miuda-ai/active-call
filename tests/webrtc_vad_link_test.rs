use active_call::app::AppStateBuilder;
use active_call::config::Config;
use active_call::event::SessionEvent;
use active_call::handler::call_router;
use active_call::media::track::file::read_wav_file;
use active_call::media::vad::VADOption;
use anyhow::{Context, Result};
use audio_codec::Encoder;
use audio_codec::g722::G722Encoder;
use futures::{SinkExt, StreamExt};
use rustrtc::{PeerConnection, RtcConfiguration, media::frame::MediaKind, media::sample_track};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::test]
async fn test_webrtc_vad_link() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    // 1. Setup AppState
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port()
    };

    let mut config = Config::default();
    config.http_addr = format!("127.0.0.1:{}", port);
    config.udp_port = 0;
    let http_addr = config.http_addr.clone();

    let app_state = AppStateBuilder::new().with_config(config).build().await?;

    let listener = TcpListener::bind(&http_addr).await?;
    let router = call_router().with_state(app_state.clone());
    let http_shutdown = CancellationToken::new();
    let http_server = {
        let shutdown = http_shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    shutdown.cancelled().await;
                })
                .await
                .ok();
        })
    };

    let app_state_clone = app_state.clone();
    tokio::spawn(async move {
        app_state_clone.serve().await.ok();
    });

    // Wait for server to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2. Connect via WebSocket
    let ws_url = format!("ws://127.0.0.1:{}/call/webrtc?id=test-vad-session", port);
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    // 3. Setup WebRTC PeerConnection (Client Side)
    let mut rtc_config = RtcConfiguration::default();
    rtc_config.ice_servers = vec![];
    let pc = Arc::new(PeerConnection::new(rtc_config));

    // Audio track (G722, 16kHz)
    let (source, track, _) = sample_track(MediaKind::Audio, 100);
    let params = rustrtc::RtpCodecParameters {
        clock_rate: 16000,
        channels: 1,
        payload_type: 9, // G722
        ..Default::default()
    };
    pc.add_track(track, params).expect("Failed to add track");

    // Create offer
    let offer = pc.create_offer().await?;
    pc.set_local_description(offer.clone())?;

    // 4. Send Invite command with VAD enabled
    let mut vad_option = VADOption::default();
    vad_option.samplerate = 16000;

    let invite_cmd = serde_json::json!({
        "command": "invite",
        "option": {
            "offer": offer.to_sdp_string(),
            "vad": vad_option
        }
    });
    ws_stream
        .send(Message::Text(invite_cmd.to_string().into()))
        .await?;

    // 5. Wait for Answer event
    let mut answer_received = false;
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg {
            if let Ok(event) = serde_json::from_str::<SessionEvent>(&text.to_string()) {
                if let SessionEvent::Answer { sdp, .. } = event {
                    let desc = rustrtc::SessionDescription::parse(rustrtc::SdpType::Answer, &sdp)?;
                    pc.set_remote_description(desc).await?;
                    answer_received = true;
                    break;
                }
            }
        }
    }
    assert!(answer_received, "Did not receive Answer event");

    // 6. Play hello_book_course_zh_16k.wav
    let (samples, sample_rate) = read_wav_file("fixtures/hello_book_course_zh_16k.wav")
        .context("Failed to read wav file")?;
    assert_eq!(sample_rate, 16000);

    let mut encoder = G722Encoder::new();
    let source_clone = source.clone();

    tokio::spawn(async move {
        info!("Starting to play audio to WebRTC link...");
        let frame_size = 320; // 20ms at 16kHz
        let mut interval = tokio::time::interval(Duration::from_millis(20));

        for chunk in samples.chunks(frame_size) {
            interval.tick().await;

            let mut chunk_vec = chunk.to_vec();
            if chunk_vec.len() < frame_size {
                chunk_vec.resize(frame_size, 0);
            }

            let encoded = encoder.encode(&chunk_vec);

            let frame = rustrtc::media::frame::AudioFrame {
                data: bytes::Bytes::from(encoded),
                clock_rate: 16000,
                payload_type: Some(9),
                ..Default::default()
            };

            if let Err(e) = source_clone.send_audio(frame).await {
                tracing::error!("Failed to send audio frame: {:?}", e);
                break;
            }
        }
        info!("Finished playing audio");
    });

    // 7. Wait for Speaking event
    let mut speaking_received = false;
    let timeout = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                if let Some(Ok(Message::Text(text))) = msg {
                    info!("WS Received: {}", text);
                    if let Ok(event) = serde_json::from_str::<SessionEvent>(&text.to_string()) {
                        match event {
                            SessionEvent::Speaking { .. } => {
                                info!("Received Speaking event!");
                                speaking_received = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                } else {
                    break;
                }
            }
            _ = &mut timeout => {
                info!("Timeout waiting for Speaking event");
                break;
            }
        }
    }

    assert!(speaking_received, "Did not receive Speaking event");

    // Cleanup
    http_shutdown.cancel();
    http_server.await.ok();

    Ok(())
}
