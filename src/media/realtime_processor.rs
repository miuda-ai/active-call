use super::processor::Processor;
use crate::{
    RealtimeOption, RealtimeType,
    event::{EventSender, SessionEvent},
    media::{AudioFrame, INTERNAL_SAMPLERATE, Samples, TrackId},
};
use anyhow::{Result, anyhow};
use audio_codec::BoxedResampler;
use base64::{Engine as _, engine::general_purpose};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, Request},
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

const REALTIME_SAMPLE_RATE: u32 = 24000;

pub struct RealtimeProcessor {
    audio_tx: mpsc::UnboundedSender<Vec<i16>>,
}

impl RealtimeProcessor {
    pub fn new(
        track_id: TrackId,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        packet_sender: mpsc::UnboundedSender<AudioFrame>,
        option: RealtimeOption,
    ) -> Result<Self> {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel::<Vec<i16>>();

        crate::spawn(async move {
            if let Err(e) = run_realtime_loop(
                track_id,
                cancel_token,
                event_sender,
                packet_sender,
                audio_rx,
                option,
            )
            .await
            {
                error!("Realtime loop failed: {:?}", e);
            }
        });

        Ok(Self { audio_tx })
    }
}

impl Processor for RealtimeProcessor {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if let Samples::PCM { samples } = &frame.samples {
            if !samples.is_empty() {
                self.audio_tx.send(samples.clone()).ok();
            }
        }
        Ok(())
    }
}

async fn run_realtime_loop(
    track_id: TrackId,
    cancel_token: CancellationToken,
    event_sender: EventSender,
    packet_sender: mpsc::UnboundedSender<AudioFrame>,
    mut audio_rx: mpsc::UnboundedReceiver<Vec<i16>>,
    option: RealtimeOption,
) -> Result<()> {
    let provider = option.provider.clone().unwrap_or(RealtimeType::OpenAI);
    let model = option.model.clone().unwrap_or_else(|| match provider {
        RealtimeType::OpenAI => "gpt-realtime-2.1".to_string(),
        _ => "gpt-4o-realtime-preview-2024-10-01".to_string(),
    });

    let url = match &provider {
        RealtimeType::OpenAI => {
            format!("wss://api.openai.com/v1/realtime?model={}", model)
        }
        RealtimeType::Azure => {
            let endpoint = option
                .endpoint
                .as_ref()
                .ok_or_else(|| anyhow!("Azure endpoint missing"))?;
            format!(
                "{}/openai/realtime?api-version=2024-10-01-preview&deployment={}",
                endpoint, model
            )
        }
        RealtimeType::Other(u) => u.clone(),
    };

    let api_key = option
        .secret_key
        .as_ref()
        .ok_or_else(|| anyhow!("API key missing"))?;

    let request = build_realtime_request(&provider, &url, &api_key)?;

    info!(url = %url, "Connecting to Realtime API");
    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let session_update = build_session_update(&provider, &option);
    ws_tx
        .send(Message::Text(session_update.to_string().into()))
        .await?;

    let mut event_rx = event_sender.subscribe();
    // OpenAI and Azure use 24 kHz PCM16. Custom endpoints use the existing 16 kHz audio.
    let mut resamplers = match provider {
        RealtimeType::OpenAI | RealtimeType::Azure => Some((
            BoxedResampler::new(INTERNAL_SAMPLERATE as usize, REALTIME_SAMPLE_RATE as usize)?,
            BoxedResampler::new(REALTIME_SAMPLE_RATE as usize, INTERNAL_SAMPLERATE as usize)?,
        )),
        RealtimeType::Other(_) => None,
    };

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            Ok(event) = event_rx.recv() => {
                match event {
                    SessionEvent::Interrupt { .. } => {
                        debug!("Interruption received, cancelling response");
                        let cancel_event = json!({
                            "type": "response.cancel"
                        });
                        ws_tx.send(Message::Text(cancel_event.to_string().into())).await?;
                    }
                    _ => {}
                }
            }
            Some(mut samples) = audio_rx.recv() => {
                if let Some((input_resampler, _)) = &mut resamplers {
                    samples = input_resampler.resample(&samples);
                }
                if samples.is_empty() {
                    continue;
                }
                let base64_audio = general_purpose::STANDARD.encode(
                    samples.iter().flat_map(|&s| s.to_le_bytes()).collect::<Vec<u8>>()
                );
                let append_event = json!({
                    "type": "input_audio_buffer.append",
                    "audio": base64_audio
                });
                ws_tx.send(Message::Text(append_event.to_string().into())).await?;
            }
            Some(msg) = ws_rx.next() => {
                let msg = msg?;
                if let Message::Text(text) = msg {
                    let v: Value = serde_json::from_str(&text)?;
                    match v["type"].as_str() {
                        Some("response.output_audio.delta") | Some("response.audio.delta") => {
                            if let Some(delta_base64) = v["delta"].as_str() {
                                if let Ok(data) = general_purpose::STANDARD.decode(delta_base64) {
                                    let mut samples: Vec<i16> = data.chunks_exact(2)
                                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                                        .collect();

                                    if let Some((_, output_resampler)) = &mut resamplers {
                                        samples = output_resampler.resample(&samples);
                                    }
                                    if samples.is_empty() {
                                        continue;
                                    }
                                    packet_sender.send(AudioFrame {
                                        track_id: "server-side-track".to_string(),
                                        samples: Samples::PCM { samples },
                                        timestamp: crate::media::get_timestamp(),
                                        sample_rate: INTERNAL_SAMPLERATE,
                                        channels: 1,
                                        ..Default::default()
                                    })?;
                                }
                            }
                        }
                        Some("input_audio_buffer.speech_started") => {
                            debug!("Speech started detected by server");
                            event_sender.send(SessionEvent::Speaking{
                                track_id: track_id.clone(),
                                timestamp: crate::media::get_timestamp(),
                                start_time: v["audio_start_ms"].as_u64().unwrap_or_default(),
                                is_filler: None,
                                confidence: None,
                                refer: None,
                            }).ok();

                            // Immediately signal interruption to stop current local playback
                            event_sender.send(SessionEvent::Interrupt {
                                receiver: Some(track_id.clone()),
                            }).ok();
                        }
                        Some("response.output_audio_transcript.delta") | Some("response.audio_transcript.delta") => {
                            if let Some(delta) = v["delta"].as_str() {
                                event_sender.send(SessionEvent::AsrDelta {
                                    track_id: track_id.clone(),
                                    index: v["content_index"].as_u64().unwrap_or_default() as u32,
                                    text: delta.to_string(),
                                    timestamp: crate::media::get_timestamp(),
                                    task_id: Some(v["item_id"].as_str().unwrap_or_default().to_string()),
                                    start_time: None,
                                    end_time: None,
                                    is_filler: None,
                                    confidence: None,
                                    refer: None,
                                }).ok();
                            }
                        }
                        Some("response.function_call_arguments.done") => {
                            if let (Some(name), Some(args), Some(call_id)) = (
                                v["name"].as_str(),
                                v["arguments"].as_str(),
                                v["call_id"].as_str(),
                            ) {
                                debug!(name, args, call_id, "Function call detected");
                                event_sender.send(SessionEvent::FunctionCall {
                                    track_id: track_id.clone(),
                                    call_id: call_id.to_string(),
                                    name: name.to_string(),
                                    arguments: args.to_string(),
                                    timestamp: crate::media::get_timestamp(),
                                }).ok();
                            }
                        }
                        Some("error") => {
                            error!("Realtime error: {}", v["error"]["message"]);
                        }
                        _ => {
                            debug!(msg_type = ?v["type"], "Other Realtime event");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn build_session_update(provider: &RealtimeType, option: &RealtimeOption) -> Value {
    let instructions = option
        .extra
        .as_ref()
        .and_then(|e| e.get("instructions"))
        .cloned()
        .unwrap_or_default();
    let voice = option
        .extra
        .as_ref()
        .and_then(|e| e.get("voice"))
        .cloned()
        .unwrap_or_else(|| "alloy".to_string());
    let turn_detection = option.turn_detection.clone().unwrap_or(json!({
        "type": "server_vad",
        "threshold": 0.5,
        "prefix_padding_ms": 300,
        "silence_duration_ms": 500
    }));
    let tools = option.tools.clone().unwrap_or_default();
    let session = match provider {
        RealtimeType::OpenAI => json!({
            "type": "realtime",
            "output_modalities": ["audio"],
            "instructions": instructions,
            "audio": {
                "input": {
                    "format": {"type": "audio/pcm", "rate": REALTIME_SAMPLE_RATE},
                    "turn_detection": turn_detection
                },
                "output": {
                    "format": {"type": "audio/pcm", "rate": REALTIME_SAMPLE_RATE},
                    "voice": voice
                }
            },
            "tools": tools
        }),
        _ => json!({
            "modalities": ["text", "audio"],
            "instructions": instructions,
            "voice": voice,
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "turn_detection": turn_detection,
            "tools": tools
        }),
    };
    json!({"type": "session.update", "session": session})
}

/// Build the websocket handshake request for the realtime provider.
///
/// Starts from the standard `&str` conversion so tungstenite generates the
/// required handshake headers (Host / Connection / Upgrade /
/// Sec-WebSocket-Version / Sec-WebSocket-Key), then layers the
/// provider-specific auth headers on top. A bare
/// `http::Request::builder().uri(..)` would pass through
/// `IntoClientRequest` untouched (the `Request` impl is a no-op) and the
/// upgrade would fail for lack of those headers.
fn build_realtime_request(
    provider: &RealtimeType,
    url: &str,
    api_key: &str,
) -> Result<Request<()>> {
    let mut request = url.into_client_request()?;
    let headers = request.headers_mut();
    match provider {
        RealtimeType::OpenAI => {
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {}", api_key))?,
            );
        }
        RealtimeType::Azure => {
            headers.insert("api-key", HeaderValue::from_str(api_key)?);
        }
        RealtimeType::Other(_) => {}
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'r>(request: &'r Request<()>, name: &str) -> Option<&'r str> {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    #[test]
    fn openai_request_carries_required_websocket_headers() {
        let request = build_realtime_request(
            &RealtimeType::OpenAI,
            "wss://api.openai.com/v1/realtime?model=gpt-realtime-2.1",
            "sk-test",
        )
        .unwrap();
        for name in [
            "Host",
            "Connection",
            "Upgrade",
            "Sec-WebSocket-Version",
            "Sec-WebSocket-Key",
        ] {
            assert!(
                header(&request, name).is_some(),
                "missing handshake header: {name}"
            );
        }
        assert_eq!(header(&request, "Authorization"), Some("Bearer sk-test"));
        assert!(header(&request, "OpenAI-Beta").is_none());
    }

    #[test]
    fn azure_request_carries_api_key() {
        let request = build_realtime_request(
            &RealtimeType::Azure,
            "wss://endpoint/openai/realtime",
            "az-key",
        )
        .unwrap();
        for name in ["Host", "Connection", "Upgrade", "Sec-WebSocket-Key"] {
            assert!(
                header(&request, name).is_some(),
                "missing handshake header: {name}"
            );
        }
        assert_eq!(header(&request, "api-key"), Some("az-key"));
    }
    #[test]
    fn openai_session_uses_ga_audio_settings() {
        let mut option = RealtimeOption::default();
        option.extra = Some(
            [
                ("instructions".to_string(), "Answer briefly.".to_string()),
                ("voice".to_string(), "marin".to_string()),
            ]
            .into(),
        );
        option.turn_detection = Some(json!({"type": "semantic_vad"}));
        option.tools = Some(vec![json!({"type": "function", "name": "hangup"})]);
        let event = build_session_update(&RealtimeType::OpenAI, &option);
        let session = &event["session"];
        assert_eq!(session["type"], "realtime");
        assert_eq!(session["output_modalities"], json!(["audio"]));
        assert_eq!(
            session["audio"]["input"]["format"],
            json!({"type": "audio/pcm", "rate": 24000})
        );
        assert_eq!(
            session["audio"]["output"]["format"],
            json!({"type": "audio/pcm", "rate": 24000})
        );
        assert_eq!(session["audio"]["output"]["voice"], "marin");
        assert_eq!(
            session["audio"]["input"]["turn_detection"]["type"],
            "semantic_vad"
        );
        assert_eq!(session["instructions"], "Answer briefly.");
        assert_eq!(session["tools"], json!(option.tools.unwrap()));
        assert!(session.get("modalities").is_none());
        assert!(session.get("input_audio_format").is_none());
    }

    #[test]
    fn azure_session_retains_legacy_configuration() {
        let event = build_session_update(&RealtimeType::Azure, &RealtimeOption::default());
        assert_eq!(event["session"]["modalities"], json!(["text", "audio"]));
        assert_eq!(event["session"]["input_audio_format"], "pcm16");
        assert_eq!(event["session"]["voice"], "alloy");
        assert!(event["session"].get("audio").is_none());
    }

    #[tokio::test]
    async fn realtime_pcm_matches_api_rate_and_preserves_transcript_events() {
        use tokio::time::{Duration, timeout};
        for custom_endpoint in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("ws://{}", listener.local_addr().unwrap());
            let (provider, wire_sample_count, audio_delta, transcript_delta) = if custom_endpoint {
                (
                    RealtimeType::Other(endpoint.clone()),
                    1600,
                    "response.audio.delta",
                    "response.audio_transcript.delta",
                )
            } else {
                (
                    RealtimeType::Azure,
                    2400,
                    "response.output_audio.delta",
                    "response.output_audio_transcript.delta",
                )
            };
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                ws.next().await.unwrap().unwrap();
                let append = ws.next().await.unwrap().unwrap();
                let append: Value = serde_json::from_str(append.to_text().unwrap()).unwrap();
                assert_eq!(append["type"], "input_audio_buffer.append");
                let input = general_purpose::STANDARD
                    .decode(append["audio"].as_str().unwrap())
                    .unwrap();
                assert_eq!(
                    input.len(),
                    wire_sample_count * 2,
                    "100 ms of input must be {wire_sample_count} PCM16 samples"
                );
                let output: Vec<u8> = (0..wire_sample_count as i16)
                    .flat_map(i16::to_le_bytes)
                    .collect();
                ws.send(Message::Text(
                    json!({
                        "type": audio_delta,
                        "delta": general_purpose::STANDARD.encode(output)
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
                ws.send(Message::Text(
                    json!({
                        "type": transcript_delta,
                        "delta": "Hello.", "content_index": 0, "item_id": "test-item"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
                let _ = ws.next().await;
            });
            let option = RealtimeOption {
                provider: Some(provider),
                endpoint: Some(endpoint),
                secret_key: Some("test-key".to_string()),
                ..Default::default()
            };
            let (events, mut event_rx) = tokio::sync::broadcast::channel(16);
            let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            let mut processor = RealtimeProcessor::new(
                "caller".to_string(),
                cancel.clone(),
                events,
                packet_tx,
                option,
            )
            .unwrap();
            processor
                .process_frame(&mut AudioFrame {
                    samples: Samples::PCM {
                        samples: vec![100; 1600],
                    },
                    ..Default::default()
                })
                .unwrap();
            let packet = timeout(Duration::from_secs(3), packet_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(packet.sample_rate, INTERNAL_SAMPLERATE);
            let Samples::PCM { samples } = packet.samples else {
                panic!("expected PCM")
            };
            assert_eq!(
                samples.len(),
                1600,
                "100 ms of output must be 1600 internal samples"
            );
            let transcript = timeout(Duration::from_secs(3), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(transcript, SessionEvent::AsrDelta { text, task_id, .. }
                if text == "Hello." && task_id.as_deref() == Some("test-item"))
            );
            cancel.cancel();
            server.await.unwrap();
        }
    }
}
