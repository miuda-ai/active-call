use crate::{
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::Samples,
    media::TrackId,
    media::{
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use audio_codec::{bytes_to_samples, resample, samples_to_bytes};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt, stream::SplitSink};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

type WsConn = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsConn, Message>;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MediaPassOption {
    url: String,             // websocket url, e.g. ws://localhost:8080/
    input_sample_rate: u32,  // sample rate of audio receiving from websocket
    output_sample_rate: u32, // sample rate of audio sending to websocket server
    /// Size in bytes of each binary frame sent to the websocket server (uplink
    /// send buffer). Defaults to [`DEFAULT_BUFFER_MS`] worth of audio at
    /// `output_sample_rate`.
    packet_size: Option<u32>,
    /// Fixed interval in milliseconds at which buffered websocket audio is
    /// re-emitted towards the call. Since one frame becomes one RTP packet this
    /// is also the downlink packet duration. Defaults to [`DEFAULT_PTIME_MS`];
    /// longer explicit values are clamped to [`MAX_PTIME_MS`]. `Some(0)`
    /// disables buffering and emits every websocket message as it arrives.
    ptime: Option<u32>,
}

/// Default uplink chunking window, in milliseconds: when the caller does not
/// specify `packet_size`, each binary frame sent to the websocket server
/// carries this much audio.
pub const DEFAULT_BUFFER_MS: u32 = 100;

/// Default downlink emit period, in milliseconds, used when the caller does not
/// specify `ptime`. This doubles as the RTP packet duration, so it follows the
/// 20 ms voice-packet norm; a 100 ms window made every call send 100 ms of
/// audio per RTP packet, which starves the far end's jitter buffer.
pub const DEFAULT_PTIME_MS: u32 = 20;

/// Upper bound applied to an explicit `ptime`. Each tick becomes a single RTP
/// packet, so longer frames reach the peer as bursts with a long gap between
/// them and underrun its jitter buffer.
pub const MAX_PTIME_MS: u32 = 40;

/// Bytes of 16-bit mono PCM covering [`DEFAULT_BUFFER_MS`] at `sample_rate`.
///
/// `sample_rate` samples/s * 0.1 s * 2 bytes/sample == `sample_rate / 5`.
fn default_buffer_bytes(sample_rate: u32) -> u32 {
    (sample_rate / 5).max(1)
}

#[cfg(test)]
mod default_buffer_tests {
    use super::*;

    #[test]
    fn default_buffer_is_100ms_of_pcm16_mono() {
        // 100 ms == sample_rate / 10 samples * 2 bytes.
        assert_eq!(default_buffer_bytes(8000), 1600);
        assert_eq!(default_buffer_bytes(16000), 3200);
        assert_eq!(default_buffer_bytes(48000), 9600);
        // Never zero, even for a degenerate rate.
        assert_eq!(default_buffer_bytes(0), 1);
    }

    #[test]
    fn explicit_packet_size_wins_and_zero_falls_back() {
        let opt = MediaPassOption::new("ws://x".into(), 16000, 16000, Some(640), Some(20));
        let track = MediaPassTrack::new("s".into(), 0, "t".into(), CancellationToken::new(), opt);
        assert_eq!(track.packet_size, 640);
        assert_eq!(track.config.ptime, Duration::from_millis(20));

        let opt = MediaPassOption::new("ws://x".into(), 16000, 16000, Some(0), None);
        let track = MediaPassTrack::new("s".into(), 0, "t".into(), CancellationToken::new(), opt);
        assert_eq!(
            track.packet_size, 3200,
            "0 falls back to the 100ms uplink default"
        );
        assert_eq!(
            track.config.ptime,
            Duration::from_millis(DEFAULT_PTIME_MS as u64)
        );
    }

    #[test]
    fn default_packet_size_tracks_output_sample_rate() {
        let opt = MediaPassOption::new("ws://x".into(), 8000, 8000, None, None);
        let track = MediaPassTrack::new("s".into(), 0, "t".into(), CancellationToken::new(), opt);
        assert_eq!(track.packet_size, 1600);
        assert_eq!(
            track.config.ptime,
            Duration::from_millis(DEFAULT_PTIME_MS as u64)
        );
    }

    #[test]
    fn downlink_defaults_to_a_20ms_rtp_period() {
        // The production shape: caller only sets the uplink packet size.
        let opt = MediaPassOption::new("ws://x".into(), 16000, 16000, Some(2560), None);
        let track = MediaPassTrack::new("s".into(), 0, "t".into(), CancellationToken::new(), opt);
        assert_eq!(track.config.ptime, Duration::from_millis(20));
        assert_eq!(
            track.config.samplerate * track.config.ptime.as_millis() as u32 / 500,
            640,
            "20 ms at 16 kHz/16-bit is one 640 byte frame"
        );
    }

    #[test]
    fn explicit_ptime_is_clamped_and_zero_stays_passthrough() {
        let opt = MediaPassOption::new("ws://x".into(), 16000, 16000, None, Some(100));
        let track = MediaPassTrack::new("s".into(), 0, "t".into(), CancellationToken::new(), opt);
        assert_eq!(
            track.config.ptime,
            Duration::from_millis(MAX_PTIME_MS as u64),
            "long frames would become long RTP packets"
        );

        let opt = MediaPassOption::new("ws://x".into(), 16000, 16000, None, Some(0));
        let track = MediaPassTrack::new("s".into(), 0, "t".into(), CancellationToken::new(), opt);
        assert_eq!(track.config.ptime, Duration::ZERO);
    }

    fn pacer_tick(pacer: &mut DownlinkPacer, buffer: &mut BytesMut) -> Option<usize> {
        let mut out = vec![0u8; pacer.capacity];
        pacer.tick(buffer, &mut out)
    }

    #[test]
    fn pacer_waits_one_tick_for_the_cushion_then_emits_full_frames() {
        let mut pacer = DownlinkPacer::new(640);
        let mut buffer = BytesMut::from(&[1u8; 640][..]);
        assert_eq!(
            pacer_tick(&mut pacer, &mut buffer),
            None,
            "1 frame is not a cushion"
        );
        buffer.extend_from_slice(&[2u8; 640]);
        assert_eq!(pacer_tick(&mut pacer, &mut buffer), Some(640));
        assert_eq!(pacer.stats(), (0, 0, 1));
    }

    #[test]
    fn pacer_stays_silent_until_the_first_audio_arrives() {
        let mut pacer = DownlinkPacer::new(640);
        let mut buffer = BytesMut::new();
        for _ in 0..10 {
            assert_eq!(
                pacer_tick(&mut pacer, &mut buffer),
                None,
                "an empty source must not be padded into the stream"
            );
        }

        // A full 80 ms websocket chunk immediately satisfies the cushion.
        buffer.extend_from_slice(&[4u8; 2560]);
        assert_eq!(pacer_tick(&mut pacer, &mut buffer), Some(640));
        assert_eq!(pacer.stats(), (0, 0, 1));
    }

    #[test]
    fn pacer_pads_and_counts_underruns_once_started() {
        let mut pacer = DownlinkPacer::new(640);
        let mut buffer = BytesMut::from(&[1u8; 1280][..]);
        assert_eq!(pacer_tick(&mut pacer, &mut buffer), Some(640));
        assert_eq!(pacer_tick(&mut pacer, &mut buffer), Some(640));

        let mut out = vec![0u8; 640];
        assert_eq!(pacer.tick(&mut buffer, &mut out), Some(0), "never re-waits");
        assert!(out.iter().all(|b| *b == 0));
        assert_eq!(pacer.stats(), (640, 1, 3));
    }

    #[test]
    fn pacer_gives_up_waiting_so_short_utterances_still_play() {
        let mut pacer = DownlinkPacer::new(640);
        let mut buffer = BytesMut::from(&[7u8; 640][..]);
        assert_eq!(pacer_tick(&mut pacer, &mut buffer), None);
        assert_eq!(pacer_tick(&mut pacer, &mut buffer), None);
        assert_eq!(pacer_tick(&mut pacer, &mut buffer), Some(640));
    }

    #[test]
    fn pacer_never_starves_on_80ms_websocket_chunks() {
        // The production source: 2560 bytes (80 ms) every fourth 20 ms tick.
        let mut pacer = DownlinkPacer::new(640);
        let mut buffer = BytesMut::new();
        let mut emitted = 0;
        for tick in 0..50 {
            if tick % 4 == 0 {
                buffer.extend_from_slice(&[3u8; 2560]);
            }
            if pacer_tick(&mut pacer, &mut buffer).is_some() {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 50, "every tick must emit");
        assert_eq!(
            pacer.stats().0,
            0,
            "an 80 ms source never starves a 20 ms pacer"
        );
    }
}

impl MediaPassOption {
    pub fn new(
        url: String,
        input_sample_rate: u32,
        output_sample_rate: u32,
        packet_size: Option<u32>,
        ptime: Option<u32>,
    ) -> Self {
        Self {
            url,
            input_sample_rate,
            output_sample_rate,
            packet_size,
            ptime,
        }
    }
}

pub struct MediaPassTrack {
    session_id: String,
    track_id: TrackId,
    cancel_token: CancellationToken,
    config: TrackConfig, // input sample rate is here, it is 0 if the ptime is None
    url: String,
    output_sample_rate: u32, // output sample rate
    packet_size: u32,
    // buffer the rtp/webrtc packets, send to websocket server with packet size
    buffer: Mutex<BytesMut>,
    ws_sink: Arc<Mutex<Option<WsSink>>>,
    bytes_sent: Arc<AtomicU64>, // bytes sent to websocket
    ssrc: u32,
    processor_chain: ProcessorChain,
}

/// Downlink pacing state machine for the buffered websocket audio.
///
/// Every tick emits exactly one `ptime` worth of 16-bit mono PCM (and therefore
/// exactly one RTP packet of a regular duration). The first emission waits for
/// a two-tick cushion so a real-time source does not start on a tick-by-tick
/// race; once playout has started the pacer never waits again — it pads with
/// silence and counts the underrun instead, so a late websocket frame cannot
/// stall the stream.
struct DownlinkPacer {
    /// Bytes emitted per tick.
    capacity: usize,
    /// Bytes that must be buffered before the first emission.
    prefill: usize,
    /// Ticks spent waiting for the prefill cushion.
    prefill_ticks: u32,
    started: bool,
    ticks: u64,
    underrun_ticks: u64,
    padded_bytes: u64,
}

impl DownlinkPacer {
    /// Stop waiting for the cushion after this many ticks so a short utterance
    /// is still played instead of being held back forever.
    const MAX_PREFILL_TICKS: u32 = 2;

    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            prefill: capacity.saturating_mul(2),
            prefill_ticks: 0,
            started: false,
            ticks: 0,
            underrun_ticks: 0,
            padded_bytes: 0,
        }
    }

    /// Fill `out` for one tick, returning the number of bytes taken from
    /// buffered audio (`out` is zero padded up to `capacity`). Returns `None`
    /// while the initial prefill cushion is still being accumulated, and while
    /// nothing has arrived yet (a silent tick before the first websocket frame
    /// must not be padded into the stream).
    fn tick(&mut self, buffer: &mut BytesMut, out: &mut [u8]) -> Option<usize> {
        if !self.started {
            if buffer.is_empty() {
                return None;
            }
            if buffer.len() < self.prefill && self.prefill_ticks < Self::MAX_PREFILL_TICKS {
                self.prefill_ticks += 1;
                return None;
            }
            self.started = true;
        }

        out.fill(0);
        let capacity = self.capacity;
        let len = buffer.len().min(capacity).min(out.len());
        if len > 0 {
            let cut = buffer.split_to(len);
            out[..len].copy_from_slice(&cut);
        }

        self.ticks += 1;
        let padded = capacity - len;
        if padded > 0 {
            self.underrun_ticks += 1;
            self.padded_bytes += padded as u64;
        }
        Some(len)
    }

    /// `(padded_bytes, underrun_ticks, ticks)` for the end-of-track summary.
    fn stats(&self) -> (u64, u64, u64) {
        (self.padded_bytes, self.underrun_ticks, self.ticks)
    }
}

impl MediaPassTrack {
    pub fn new(
        session_id: String,
        ssrc: u32,
        track_id: TrackId,
        cancel_token: CancellationToken,
        option: MediaPassOption,
    ) -> Self {
        let sample_rate = option.output_sample_rate;
        // `inputSampleRate` is sometimes omitted as 0 when ptime is disabled.
        // A 0 here used to flow into the track's frames and make
        // `ProcessorChain` build a 0 Hz resampler, panicking inside the media
        // worker. Fall back to the output rate so the pipeline always has a
        // valid source rate.
        let input_sample_rate = if option.input_sample_rate > 0 {
            option.input_sample_rate
        } else {
            warn!(
                input_sample_rate = option.input_sample_rate,
                output_sample_rate = sample_rate,
                "media pass input sample rate is 0, falling back to output sample rate"
            );
            sample_rate
        };
        let mut config = TrackConfig::default();
        config = config.with_sample_rate(input_sample_rate);
        // Downlink emit period == RTP packet duration. Callers that only set
        // `packet_size` (an *uplink* chunk size) used to inherit the 100 ms
        // buffering window here, so the whole call went out as 100 ms RTP
        // packets. Clamp explicit values for the same reason.
        let ptime_ms = match option.ptime {
            Some(0) => 0,
            Some(ms) => ms.min(MAX_PTIME_MS),
            None => DEFAULT_PTIME_MS,
        };
        config = config.with_ptime(Duration::from_millis(ptime_ms as u64));
        // Uplink send buffer: chunk websocket sends into `packet_size` bytes.
        // Default to a 100 ms window at the output rate (3200 bytes @16 kHz,
        // 1600 bytes @8 kHz); an explicit 0 falls back to the same default.
        let packet_size = match option.packet_size {
            Some(size) if size > 0 => size,
            _ => default_buffer_bytes(sample_rate),
        };
        let buffer: Mutex<BytesMut> = Mutex::new(BytesMut::with_capacity(packet_size as usize * 2));
        Self {
            session_id,
            track_id,
            cancel_token,
            config,
            url: option.url,
            output_sample_rate: sample_rate,
            packet_size,
            buffer,
            ssrc,
            ws_sink: Arc::new(Mutex::new(None)),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            // dummy processor chain, will ignore processor
            processor_chain: ProcessorChain::new(sample_rate),
        }
    }
}

#[async_trait]
impl Track for MediaPassTrack {
    fn ssrc(&self) -> u32 {
        self.ssrc
    }

    fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        // The chain is still used to process this track's own frames, and it
        // carries the recorder tap: `MediaStream` attaches the recorder through
        // this accessor so it bypasses the processor-ignoring overrides below.
        &mut self.processor_chain
    }
    fn insert_processor(&mut self, _: Box<dyn Processor>) {
        // Audio processors (denoise/VAD/ASR…) must not touch the passthrough
        // media; the recorder reaches the chain via `processor_chain()`.
        warn!(track_id = %self.track_id, "ignore processor for media pass track");
    }
    fn append_processor(&mut self, _: Box<dyn Processor>) {
        warn!(track_id = %self.track_id, "ignore processor for media pass track");
    }
    async fn handshake(&mut self, _: String, _: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
    }

    async fn start(
        &mut self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let mut url = url::Url::parse(&self.url)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("sample_rate", self.output_sample_rate.to_string().as_str());
            query.append_pair("packet_size", self.packet_size.to_string().as_str());
        }
        info!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            input_sample_rate = self.config.samplerate,
            output_sample_rate = self.output_sample_rate,
            packet_size = self.packet_size,
            ptime_ms = self.config.ptime.as_millis(),
            "Media pass track starting"
        );
        let input_sample_rate = self.config.samplerate;
        let output_sample_rate = self.output_sample_rate;
        let (ws_stream, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
        let (ws_sink, mut ws_source) = ws_stream.split();
        *self.ws_sink.lock().await = Some(ws_sink);
        let ws_sink = self.ws_sink.clone();
        let bytes_sent = self.bytes_sent.clone();
        let session_id = self.session_id.clone();
        let ssrc = self.ssrc;
        let track_id = self.track_id.clone();
        let start_time = crate::media::get_timestamp();
        let cancel_token = self.cancel_token.clone();
        let ptime = self.config.ptime;
        let ptime_ms = ptime.as_millis() as u32;
        let channels = self.config.channels;
        let mut processor_chain = self.processor_chain.clone();
        crate::spawn(async move {
            let mut bytes_received = 0u64;
            let mut bytes_emitted = 0u64;

            // ptimer is polled only if ptime > 0
            let capacity = input_sample_rate as usize * ptime_ms as usize / 500;
            let (mut ptimer, mut samples, mut buffer) = if ptime_ms > 0 {
                (
                    tokio::time::interval(Duration::from_millis(ptime_ms as u64)),
                    vec![0u8; capacity],
                    BytesMut::with_capacity(8 * 1024),
                )
            } else {
                (
                    tokio::time::interval(Duration::MAX),
                    Vec::new(),
                    BytesMut::new(),
                )
            };
            let mut pacer = DownlinkPacer::new(capacity);

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        info!(session_id, "Media pass track cancelled");
                        break;
                    }
                    _ = ptimer.tick(), if ptime_ms > 0 => {
                        // One fixed-size frame per tick; silence-padded if the
                        // websocket is behind. `None` means we are still
                        // building the initial cushion.
                        let Some(filled) = pacer.tick(&mut buffer, &mut samples) else {
                            continue;
                        };

                        // Create frame (will have zeros if not enough data)
                        let samples_vec = bytes_to_samples(&samples[..]);
                        let mut frame = AudioFrame {
                            track_id: track_id.clone(),
                            samples: Samples::PCM { samples: samples_vec.clone() },
                            timestamp: crate::media::get_timestamp(),
                            sample_rate: input_sample_rate,
                            channels,
                            ..Default::default()
                        };

                        if let Err(e) = processor_chain.process_frame(&mut frame) {
                            warn!(track_id, "error processing frame: {}", e);
                        }

                        if let Ok(_) = packet_sender.send(frame) {
                            // only count the actual bytes filled from buffer
                            bytes_emitted += filled as u64;
                        } else {
                            warn!(
                                track_id,
                                "packet sender closed, stopping emit loop"
                            );
                            break;
                        }
                    }
                    msg = ws_source.next() => {
                        match msg {
                            Some(Ok(Message::Binary(data))) => {
                                bytes_received += data.len() as u64;
                                if ptime_ms > 0 {
                                    // buffer if ptime was set
                                    buffer.reserve(data.len());
                                    buffer.extend_from_slice(&data);
                                } else {
                                    // send immediately if ptime was not set
                                    let samples_vec = bytes_to_samples(&data);
                                    let mut frame = AudioFrame {
                                        track_id: track_id.clone(),
                                        samples: Samples::PCM { samples: samples_vec.clone() },
                                        timestamp: crate::media::get_timestamp(),
                                        sample_rate: input_sample_rate,
                                        channels,
                            ..Default::default()
                                    };

                                    if let Err(e) = processor_chain.process_frame(&mut frame) {
                                        warn!(track_id, "error processing frame: {}", e);
                                    }

                                    if let Ok(_) = packet_sender.send(frame) {
                                        bytes_emitted += data.len() as u64;
                                    } else {
                                        warn!(
                                            track_id,
                                            "packet sender closed, stopping emit loop"
                                        );
                                        break;
                                    }
                                }
                            }
                            Some(Ok(Message::Close(res))) => {
                                warn!(
                                    track_id,
                                    close_reason = ?res,
                                    bytes_received,
                                    "Media pass track closed by remote"
                                );
                                break;
                            }
                            Some(Err(e)) => {
                                error!(
                                    track_id,
                                    error = %e,
                                    bytes_received,
                                    "Media pass track WebSocket error"
                                );
                                let error = SessionEvent::Error {
                                    track_id: track_id.clone(),
                                    timestamp: crate::media::get_timestamp(),
                                    sender: format!("media_pass: {}", url),
                                    error: format!("Media pass track error: {:?}", e),
                                    code: None,
                                };
                                event_sender.send(error).ok();
                                break;
                            }
                            None => {
                                info!(
                                    track_id,
                                    bytes_received,
                                    "Media pass track WebSocket stream ended"
                                );
                                break;
                            }
                            _ => {}
                        }
                    }
                }

                // Break if packet sender is closed
                if packet_sender.is_closed() {
                    break;
                }
            }

            if let Some(mut ws_sink) = ws_sink.lock().await.take() {
                ws_sink.close().await.ok();
            };

            let duration = crate::media::get_timestamp() - start_time;
            let bytes_sent_to_ws = bytes_sent.load(Ordering::Relaxed);
            let (bytes_padded, underrun_ticks, emit_ticks) = pacer.stats();
            info!(
                session_id,
                duration,
                input_sample_rate,
                output_sample_rate,
                bytes_received,
                bytes_emitted,
                bytes_sent_to_ws,
                bytes_padded,
                underrun_ticks,
                emit_ticks,
                "Media pass track ended"
            );

            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::media::get_timestamp(),
                    duration,
                    ssrc,
                    play_id: None,
                    auto_hangup: None,
                })
                .ok();
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(mut ws_sink) = self.ws_sink.lock().await.take() {
            ws_sink.close().await.ok();
        }
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&mut self, packet: &AudioFrame) -> Result<()> {
        let mut packet = packet.clone();
        if let Err(e) = self.processor_chain.process_frame(&mut packet) {
            warn!(track_id=%self.track_id, "processor_chain process_frame error: {:?}", e);
        }

        if let Some(ws_sink) = self.ws_sink.lock().await.as_mut() {
            if let Samples::PCM { samples } = &packet.samples {
                let mut buffer = self.buffer.lock().await;
                let bytes = samples_to_bytes(samples.as_slice());
                buffer.reserve(bytes.len());
                buffer.extend_from_slice(bytes.as_slice());
                while buffer.len() >= self.packet_size as usize {
                    let bytes = buffer.split_to(self.packet_size as usize).freeze();
                    let bytes = if packet.sample_rate == self.output_sample_rate {
                        bytes
                    } else {
                        let samples = bytes_to_samples(&bytes);
                        let resample =
                            resample(&samples, packet.sample_rate, self.output_sample_rate);
                        let bytes = samples_to_bytes(resample.as_slice());
                        Bytes::copy_from_slice(bytes.as_slice())
                    };
                    let bytes_len = bytes.len();
                    ws_sink.send(Message::Binary(bytes)).await?;
                    self.bytes_sent
                        .fetch_add(bytes_len as u64, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_track(input_sample_rate: u32, output_sample_rate: u32) -> MediaPassTrack {
        MediaPassTrack::new(
            "session".to_string(),
            1,
            "track".to_string(),
            CancellationToken::new(),
            MediaPassOption::new(
                "ws://localhost".to_string(),
                input_sample_rate,
                output_sample_rate,
                None,
                None,
            ),
        )
    }

    #[test]
    fn zero_input_sample_rate_falls_back_to_output() {
        assert_eq!(new_track(0, 16000).config.samplerate, 16000);
    }

    #[test]
    fn explicit_input_sample_rate_is_preserved() {
        assert_eq!(new_track(8000, 16000).config.samplerate, 8000);
    }
}
