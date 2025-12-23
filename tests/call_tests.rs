use active_call::app::AppStateBuilder;
use active_call::call::Command;
use active_call::config::Config;
use active_call::handler::call_router;
use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tokio_util::sync::CancellationToken;
use voice_engine::event::EventSender;
use voice_engine::{
    event::SessionEvent,
    media::Sample,
    media::TrackId,
    media::engine::StreamEngine,
    synthesis::{SynthesisClient, SynthesisEvent, SynthesisOption, SynthesisType},
    transcription::{TranscriptionClient, TranscriptionOption, TranscriptionType},
};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::media::Sample as WebrtcSample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

struct MockAsrClient {
    audio_tx: mpsc::UnboundedSender<Vec<Sample>>,
}

#[async_trait]
impl TranscriptionClient for MockAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        let _ = self.audio_tx.send(samples.to_vec());
        Ok(())
    }
}

struct MockAsrClientBuilder;

impl MockAsrClientBuilder {
    pub fn create(
        track_id: TrackId,
        token: CancellationToken,
        _option: TranscriptionOption,
        event_sender: EventSender,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>> {
        Box::pin(async move {
            let (audio_tx, mut audio_rx) = mpsc::unbounded_channel::<Vec<Sample>>();

            tokio::spawn(async move {
                let mut count = 0;
                while let Some(_samples) = audio_rx.recv().await {
                    count += 1;
                    if count == 10 {
                        // Send transcription after some audio
                        let event = SessionEvent::AsrFinal {
                            track_id: track_id.clone(),
                            index: 1,
                            text: "mock transcription".to_string(),
                            timestamp: voice_engine::media::get_timestamp(),
                            start_time: Some(voice_engine::media::get_timestamp()),
                            end_time: Some(voice_engine::media::get_timestamp() + 100),
                        };
                        let _ = event_sender.send(event);
                    }
                    if token.is_cancelled() {
                        break;
                    }
                }
            });

            Ok(Box::new(MockAsrClient { audio_tx }) as Box<dyn TranscriptionClient>)
        })
    }
}

struct MockTts {
    _streaming: bool,
}

#[async_trait]
impl SynthesisClient for MockTts {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Other("mock".to_string())
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (_tx, rx) = mpsc::channel(10);
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn synthesize(
        &mut self,
        _text: &str,
        _cmd_seq: Option<usize>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_webrtc_call_workflow() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    // 1. Setup StreamEngine with mock engines
    let mut stream_engine = StreamEngine::new();
    stream_engine.register_asr(
        TranscriptionType::Other("mock".to_string()),
        Box::new(MockAsrClientBuilder::create),
    );
    stream_engine.register_tts(
        SynthesisType::Other("mock".to_string()),
        |streaming, _opt| {
            Ok(Box::new(MockTts {
                _streaming: streaming,
            }) as Box<dyn SynthesisClient>)
        },
    );
    let stream_engine = Arc::new(stream_engine);

    // 2. Setup AppState
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port()
    };

    let mut config = Config::default();
    config.http_addr = format!("127.0.0.1:{}", port);
    config.udp_port = 0;
    let http_addr = config.http_addr.clone();

    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await?;

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

    // 3. Connect via WebSocket
    let ws_url = format!("ws://127.0.0.1:{}/call/webrtc?id=test-session", port);
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    // 4. Setup WebRTC PeerConnection
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let api = APIBuilder::new().with_media_engine(m).build();
    let rtc_config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    };
    let pc = Arc::new(api.new_peer_connection(rtc_config).await?);

    let audio_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: "audio/opus".to_owned(),
            ..Default::default()
        },
        "audio".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    pc.add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Create offer
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer.clone()).await?;

    // 5. Send Invite command
    let invite_cmd = serde_json::json!({
        "command": "invite",
        "option": {
            "offer": offer.sdp,
            "tts": {
                "speaker": "mock",
                "provider": "mock"
            },
            "asr": {
                "provider": "mock"
            }
        }
    });
    ws_stream
        .send(Message::Text(invite_cmd.to_string().into()))
        .await?;

    // 6. Wait for Answer event
    let mut answer_received = false;
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg {
            if let Ok(event) = serde_json::from_str::<SessionEvent>(&text.to_string()) {
                if let SessionEvent::Answer { sdp, .. } = event {
                    let desc = RTCSessionDescription::answer(sdp)?;
                    pc.set_remote_description(desc).await?;
                    answer_received = true;
                    break;
                }
            }
        }
    }
    assert!(answer_received, "Did not receive Answer event");

    // Send some dummy audio
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        for _ in 0..50 {
            // 1 second of audio
            interval.tick().await;
            let _ = audio_track
                .write_sample(&WebrtcSample {
                    data: vec![0u8; 160].into(), // dummy opus data
                    duration: Duration::from_millis(20),
                    ..Default::default()
                })
                .await;
        }
    });

    // 7. Send TTS command
    let tts_cmd = serde_json::json!({
        "command": "tts",
        "text": "Hello, this is a test",
        "speaker": "mock",
        "playId": "test-play",
        "autoHangup": false,
        "streaming": false,
        "endOfStream": true,
        "option": {
            "speaker": "mock",
            "provider": "mock"
        }
    });
    ws_stream
        .send(Message::Text(tts_cmd.to_string().into()))
        .await?;

    // 8. Verify events (e.g., TrackStart, Transcription)
    let mut track_started = false;
    let mut asr_received = false;
    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                if let Some(Ok(Message::Text(text))) = msg {
                    if let Ok(event) = serde_json::from_str::<SessionEvent>(&text.to_string()) {
                        match event {
                            SessionEvent::TrackStart { .. } => {
                                track_started = true;
                            }
                            SessionEvent::AsrFinal { text, .. } => {
                                tracing::info!("Received transcription: {}", text);
                                asr_received = true;
                            }
                            _ => {}
                        }
                        if track_started && asr_received {
                            break;
                        }
                    }
                } else {
                    break;
                }
            }
            _ = &mut timeout => {
                break;
            }
        }
    }
    assert!(track_started, "Did not receive TrackStart event");
    assert!(asr_received, "Did not receive Transcription event");

    // 9. Hangup
    let hangup_cmd = Command::Hangup {
        reason: None,
        initiator: None,
    };
    ws_stream
        .send(Message::Text(serde_json::to_string(&hangup_cmd)?.into()))
        .await?;

    http_shutdown.cancel();
    http_server.await.ok();

    Ok(())
}
