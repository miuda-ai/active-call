use super::processor::Processor;
use crate::{
    RealtimeOption, RealtimeType,
    event::{EventSender, SessionEvent},
    media::{AudioFrame, INTERNAL_SAMPLERATE, Samples, TrackId},
};
use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

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
    let provider = option.provider.unwrap_or(RealtimeType::OpenAI);
    let model = option
        .model
        .unwrap_or_else(|| "gpt-4o-realtime-preview-2024-10-01".to_string());

    let url = match provider {
        RealtimeType::OpenAI => {
            format!("wss://api.openai.com/v1/realtime?model={}", model)
        }
        RealtimeType::Azure => {
            let endpoint = option
                .endpoint
                .ok_or_else(|| anyhow!("Azure endpoint missing"))?;
            format!(
                "{}/openai/realtime?api-version=2024-10-01-preview&deployment={}",
                endpoint, model
            )
        }
        RealtimeType::Other(ref u) => u.clone(),
    };

    let api_key = option
        .secret_key
        .ok_or_else(|| anyhow!("API key missing"))?;

    let mut request_builder = http::Request::builder().uri(&url);

    match provider {
        RealtimeType::OpenAI => {
            request_builder = request_builder
                .header("Authorization", format!("Bearer {}", api_key))
                .header("OpenAI-Beta", "realtime=v1");
        }
        RealtimeType::Azure => {
            request_builder = request_builder.header("api-key", api_key);
        }
        _ => {}
    }

    let request = request_builder.body(())?;

    info!(url = %url, "Connecting to Realtime API");
    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Initialize session
    let session_update = json!({
        "type": "session.update",
        "session": {
            "modalities": ["text", "audio"],
            "instructions": option.extra.as_ref().and_then(|e| e.get("instructions")).cloned().unwrap_or_default(),
            "voice": option.extra.as_ref().and_then(|e| e.get("voice")).cloned().unwrap_or_else(|| "alloy".to_string()),
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "turn_detection": option.turn_detection.unwrap_or(json!({
                "type": "server_vad",
                "threshold": 0.5,
                "prefix_padding_ms": 300,
                "silence_duration_ms": 500
            })),
            "tools": option.tools.unwrap_or_default(),
        }
    });
    ws_tx
        .send(Message::Text(session_update.to_string().into()))
        .await?;

    let mut event_rx = event_sender.subscribe();

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
            Some(samples) = audio_rx.recv() => {
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
                        Some("response.audio.delta") => {
                            if let Some(delta_base64) = v["delta"].as_str() {
                                if let Ok(data) = general_purpose::STANDARD.decode(delta_base64) {
                                    let samples: Vec<i16> = data.chunks_exact(2)
                                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                                        .collect();

                                    packet_sender.send(AudioFrame {
                                        track_id: "server-side-track".to_string(),
                                        samples: Samples::PCM { samples },
                                        timestamp: crate::media::get_timestamp(),
                                        sample_rate: INTERNAL_SAMPLERATE,
                                        channels: 1,
                                    })?;
                                }
                            }
                        }
                        Some("input_audio_buffer.speech_started") => {
                            debug!("Speech started detected by server");
                            event_sender.send(SessionEvent::Transcription {
                                track_id: track_id.clone(),
                                text: "".to_string(),
                                is_final: false,
                                timestamp: crate::media::get_timestamp(),
                                extra: Some(json!({ "event": "speech_started" })),
                            }).ok();

                            // Immediately signal interruption to stop current local playback
                            event_sender.send(SessionEvent::Interrupt {
                                receiver: Some(track_id.clone()),
                            }).ok();
                        }
                        Some("response.audio_transcript.delta") => {
                            if let Some(delta) = v["delta"].as_str() {
                                event_sender.send(SessionEvent::Transcription {
                                    track_id: track_id.clone(),
                                    text: delta.to_string(),
                                    is_final: false,
                                    timestamp: crate::media::get_timestamp(),
                                    extra: None,
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
