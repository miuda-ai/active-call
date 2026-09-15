use super::{TranscriptionClient, TranscriptionOption, handle_wait_for_answer_with_audio_drop};
use crate::{
    event::{EventSender, SessionEvent},
    media::{Sample, SourcePacket, TrackId},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::samples_to_bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;
use uuid::Uuid;

const DEEPGRAM_LISTEN_URL: &str = "wss://api.deepgram.com/v1/listen";

type TranscriptionClientFuture =
    Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>;

struct DeepgramAsrClientInner {
    option: TranscriptionOption,
}

pub struct DeepgramAsrClient {
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    is_closed: Arc<AtomicBool>,
}

pub struct DeepgramAsrClientBuilder {
    option: TranscriptionOption,
    track_id: Option<String>,
    token: Option<CancellationToken>,
    event_sender: EventSender,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DeepgramResult {
    #[serde(rename = "type")]
    event_type: Option<String>,
    channel: Option<DeepgramChannel>,
    is_final: bool,
    speech_final: bool,
    start: Option<f32>,
    duration: Option<f32>,
    metadata: Option<DeepgramMetadata>,
}

impl Default for DeepgramResult {
    fn default() -> Self {
        Self {
            event_type: None,
            channel: None,
            is_final: false,
            speech_final: false,
            start: None,
            duration: None,
            metadata: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DeepgramChannel {
    alternatives: Vec<DeepgramAlternative>,
}

#[derive(Debug, Deserialize)]
struct DeepgramAlternative {
    transcript: String,
    confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct DeepgramMetadata {
    request_id: Option<String>,
}

#[derive(Serialize)]
struct CloseStreamCommand {
    #[serde(rename = "type")]
    event_type: &'static str,
}

impl DeepgramAsrClientBuilder {
    pub fn create(
        track_id: TrackId,
        token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> TranscriptionClientFuture {
        Box::pin(async move {
            let builder = Self::new(option, event_sender);
            builder
                .with_cancel_token(token)
                .with_track_id(track_id)
                .build()
                .await
                .map(|client| Box::new(client) as Box<dyn TranscriptionClient>)
        })
    }

    pub fn new(option: TranscriptionOption, event_sender: EventSender) -> Self {
        Self {
            option,
            token: None,
            track_id: None,
            event_sender,
        }
    }

    pub fn with_cancel_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.token = Some(cancellation_token);
        self
    }

    pub fn with_track_id(mut self, track_id: String) -> Self {
        self.track_id = Some(track_id);
        self
    }

    pub async fn build(self) -> Result<DeepgramAsrClient> {
        let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();

        let event_sender_rx = match self.option.start_when_answer {
            Some(true) => Some(self.event_sender.subscribe()),
            _ => None,
        };

        let track_id = self.track_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let token = self.token.unwrap_or_default();
        let event_sender = self.event_sender;
        let inner = DeepgramAsrClientInner {
            option: self.option,
        };
        let is_closed = Arc::new(AtomicBool::new(false));
        let client = DeepgramAsrClient {
            audio_tx,
            is_closed: Arc::clone(&is_closed),
        };

        crate::spawn(async move {
            let res = async move {
                if event_sender_rx.is_some() {
                    handle_wait_for_answer_with_audio_drop(event_sender_rx, &mut audio_rx, &token)
                        .await;

                    if token.is_cancelled() {
                        debug!("Cancelled during wait for answer");
                        return Ok::<(), anyhow::Error>(());
                    }
                }

                let ws_stream = match inner.connect_websocket(&track_id).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        warn!(
                            track_id,
                            "Failed to connect to Deepgram ASR WebSocket: {}", e
                        );
                        let _ = event_sender.send(SessionEvent::Error {
                            timestamp: crate::media::get_timestamp(),
                            track_id,
                            sender: "DeepgramAsrClient".to_string(),
                            error: format!("Failed to connect to Deepgram ASR WebSocket: {}", e),
                            code: Some(500),
                        });
                        return Err(e);
                    }
                };

                info!(%track_id, "Starting Deepgram ASR client");
                match DeepgramAsrClient::handle_websocket_message(
                    track_id.clone(),
                    ws_stream,
                    audio_rx,
                    event_sender.clone(),
                    token,
                    inner.option.refer,
                )
                .await
                {
                    Ok(_) => debug!(track_id, "Deepgram ASR websocket handling completed"),
                    Err(e) => {
                        info!(track_id, "Error in Deepgram ASR websocket handling: {}", e);
                        event_sender
                            .send(SessionEvent::Error {
                                track_id,
                                timestamp: crate::media::get_timestamp(),
                                sender: "deepgram_asr".to_string(),
                                error: e.to_string(),
                                code: None,
                            })
                            .ok();
                    }
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            is_closed.store(true, Ordering::SeqCst);
            if let Err(e) = res {
                debug!("Deepgram ASR task finished with error: {:?}", e);
            }
        });

        Ok(client)
    }
}

impl DeepgramAsrClientInner {
    async fn connect_websocket(
        &self,
        track_id: &str,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let api_key = self
            .option
            .secret_key
            .clone()
            .or_else(|| std::env::var("DEEPGRAM_API_KEY").ok())
            .ok_or_else(|| anyhow!("No DEEPGRAM_API_KEY provided"))?;

        let mut url = Url::parse(
            self.option
                .endpoint
                .as_deref()
                .unwrap_or(DEEPGRAM_LISTEN_URL),
        )?;
        let extra = self.option.extra.as_ref();
        {
            let mut query = url.query_pairs_mut();
            if extra.map(|e| !e.contains_key("model")).unwrap_or(true) {
                query.append_pair("model", self.option.model_type.as_deref().unwrap_or("nova-3"));
            }
            if extra.map(|e| !e.contains_key("language")).unwrap_or(true) {
                if let Some(language) = self.option.language.as_deref() {
                    if language != "auto" {
                        query.append_pair("language", language);
                    }
                }
            }
            if extra.map(|e| !e.contains_key("encoding")).unwrap_or(true) {
                query.append_pair("encoding", "linear16");
            }
            if extra.map(|e| !e.contains_key("sample_rate")).unwrap_or(true) {
                query.append_pair(
                    "sample_rate",
                    self.option.samplerate.unwrap_or(16000).to_string().as_str(),
                );
            }
            if extra.map(|e| !e.contains_key("channels")).unwrap_or(true) {
                query.append_pair("channels", "1");
            }
            if extra
                .map(|e| !e.contains_key("interim_results"))
                .unwrap_or(true)
            {
                query.append_pair("interim_results", "true");
            }
            if extra.map(|e| !e.contains_key("endpointing")).unwrap_or(true) {
                query.append_pair("endpointing", "300");
            }
            if extra.map(|e| !e.contains_key("smart_format")).unwrap_or(true) {
                query.append_pair("smart_format", "true");
            }
            if let Some(extra) = extra {
                for (key, value) in extra {
                    query.append_pair(key, value);
                }
            }
        }

        let mut request = url.as_str().into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Token {}", api_key).parse()?);
        let (ws_stream, response) = connect_async(request).await?;
        debug!(
            track_id,
            "Deepgram WebSocket connection established. Response: {}",
            response.status()
        );
        Ok(ws_stream)
    }
}

impl DeepgramAsrClient {
    async fn handle_websocket_message(
        track_id: TrackId,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        mut audio_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        event_sender: EventSender,
        token: CancellationToken,
        refer: Option<bool>,
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let begin_time = crate::media::get_timestamp();
        let mut final_text = String::new();
        let mut final_start_time = None;
        let mut index = 0u32;
        let mut first_result_time = None;

        let token_clone = token.clone();
        crate::spawn(async move {
            let mut keep_alive = tokio::time::interval(Duration::from_secs(5));
            keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = token_clone.cancelled() => {
                        break;
                    }
                    _ = keep_alive.tick() => {
                        if let Err(e) = ws_sender.send(Message::Text(r#"{"type":"KeepAlive"}"#.into())).await {
                            warn!("Failed to send Deepgram KeepAlive message: {}", e);
                            break;
                        }
                    }
                    audio_data = audio_rx.recv() => {
                        match audio_data {
                            Some(audio_data) => {
                                if audio_data.is_empty() {
                                    continue;
                                }
                                if let Err(e) = ws_sender.send(Message::Binary(audio_data.into())).await {
                                    warn!("Failed to send audio data to Deepgram: {}", e);
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }

            let close_msg = CloseStreamCommand {
                event_type: "CloseStream",
            };
            if let Ok(msg_json) = serde_json::to_string(&close_msg) {
                if let Err(e) = ws_sender.send(Message::Text(msg_json.into())).await {
                    warn!("Failed to send Deepgram CloseStream message: {}", e);
                }
            }
        });

        loop {
            tokio::select! {
                msg = ws_receiver.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<DeepgramResult>(&text) {
                                Ok(result) => {
                                    if result.event_type.as_deref().is_some_and(|event_type| event_type != "Results") {
                                        continue;
                                    }

                                    let Some(alternative) = result
                                        .channel
                                        .as_ref()
                                        .and_then(|channel| channel.alternatives.first()) else {
                                            continue;
                                        };
                                    let transcript = alternative.transcript.trim();
                                    if transcript.is_empty() {
                                        continue;
                                    }

                                    let start_time = result.start.map(|start| {
                                        begin_time + (start * 1000.0).max(0.0) as u64
                                    });
                                    let end_time = result.start.zip(result.duration).map(|(start, duration)| {
                                        begin_time + ((start + duration) * 1000.0).max(0.0) as u64
                                    });

                                    if result.is_final {
                                        if final_start_time.is_none() {
                                            final_start_time = start_time;
                                        }
                                        if !final_text.is_empty() {
                                            final_text.push(' ');
                                        }
                                        final_text.push_str(transcript);
                                    }

                                    let text = if result.speech_final {
                                        if !result.is_final {
                                            if final_start_time.is_none() {
                                                final_start_time = start_time;
                                            }
                                            if !final_text.is_empty() {
                                                final_text.push(' ');
                                            }
                                            final_text.push_str(transcript);
                                        }
                                        let text = final_text.trim().to_string();
                                        final_text.clear();
                                        text
                                    } else if result.is_final {
                                        final_text.clone()
                                    } else if final_text.is_empty() {
                                        transcript.to_string()
                                    } else {
                                        format!("{} {}", final_text, transcript)
                                    };

                                    if text.is_empty() {
                                        continue;
                                    }

                                    let timestamp = crate::media::get_timestamp();
                                    let task_id = result
                                        .metadata
                                        .as_ref()
                                        .and_then(|metadata| metadata.request_id.clone());
                                    let event = if result.speech_final {
                                        SessionEvent::AsrFinal {
                                            track_id: track_id.clone(),
                                            index,
                                            text,
                                            timestamp,
                                            start_time: final_start_time.or(start_time),
                                            end_time,
                                            is_filler: None,
                                            confidence: alternative.confidence,
                                            task_id,
                                            refer,
                                        }
                                    } else {
                                        SessionEvent::AsrDelta {
                                            track_id: track_id.clone(),
                                            index,
                                            text,
                                            timestamp,
                                            start_time: final_start_time.or(start_time),
                                            end_time,
                                            is_filler: None,
                                            confidence: alternative.confidence,
                                            task_id,
                                            refer,
                                        }
                                    };
                                    event_sender.send(event).ok();

                                    let first_time = first_result_time.get_or_insert(timestamp);
                                    let metrics_key = if result.speech_final {
                                        "completed.asr.deepgram"
                                    } else {
                                        "ttfb.asr.deepgram"
                                    };
                                    event_sender
                                        .send(SessionEvent::Metrics {
                                            timestamp,
                                            key: metrics_key.to_string(),
                                            data: serde_json::json!({ "index": index }),
                                            duration: (timestamp - *first_time) as u32,
                                        })
                                        .ok();

                                    if result.speech_final {
                                        index += 1;
                                        final_start_time = None;
                                        first_result_time = None;
                                    }
                                }
                                Err(e) => {
                                    warn!(track_id, "Failed to parse Deepgram ASR response: {} {}", e, text);
                                }
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!(track_id, "Deepgram WebSocket connection closed: {:?}", frame);
                            break;
                        }
                        Some(Err(e)) => {
                            return Err(anyhow!("Deepgram WebSocket error: {}", e));
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                _ = token.cancelled() => {
                    break;
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl TranscriptionClient for DeepgramAsrClient {
    fn send_audio(&self, samples: &[Sample], _src_packet: Option<&SourcePacket>) -> Result<()> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.audio_tx
            .send(samples_to_bytes(samples))
            .map_err(|_| {
                self.is_closed.store(true, Ordering::SeqCst);
            })
            .ok();
        Ok(())
    }
}
