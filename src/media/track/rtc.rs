use super::track_codec::{TrackCodec, duration_to_rtp_ticks};
use crate::{
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::{
        jitter::{Playout, PlayoutBuffer, PlayoutConfig},
        plc::Concealer,
        processor::ProcessorChain,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use audio_codec::CodecType;
use audio_codec::telephone_event::encode_event_payload;
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use rustrtc::{
    AudioCapability, IceCandidate, IceServer, MediaKind, PeerConnection, PeerConnectionEvent,
    PeerConnectionState, RtcConfiguration, RtpCodecParameters, SdpType, TransportMode,
    config::MediaCapabilities,
    media::{
        MediaStreamTrack, SampleStreamSource, frame::AudioFrame as RtcAudioFrame, sample_track,
        track::SampleStreamTrack,
    },
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Clone)]
pub struct RtcTrackConfig {
    pub mode: TransportMode,
    pub ice_servers: Option<Vec<IceServer>>,
    pub external_ip: Option<String>,
    pub rtp_port_range: Option<(u16, u16)>,
    pub bind_ip: Option<String>,
    pub preferred_codec: Option<CodecType>,
    pub codecs: Vec<CodecType>,
    pub payload_type: Option<u8>,
    pub enable_latching: Option<bool>,
    pub enable_ice_lite: Option<bool>,
    /// Emit `RtpTimeout` when no audio arrives for this long once the call
    /// is answered; None or zero disables it.
    pub rtp_timeout: Option<Duration>,
    /// Play inbound audio out through a jitter buffer, concealing lost
    /// frames (see `media::jitter`). None: on for WebRTC, whose peers are
    /// browsers on whatever network, off for plain RTP trunks.
    pub jitter_buffer: Option<bool>,
}

impl Default for RtcTrackConfig {
    fn default() -> Self {
        Self {
            mode: TransportMode::WebRtc, // Default WebRTC behavior
            ice_servers: None,
            external_ip: None,
            rtp_port_range: None,
            bind_ip: None,
            preferred_codec: None,
            codecs: Vec::new(),
            payload_type: None,
            enable_latching: None,
            enable_ice_lite: None,
            rtp_timeout: None,
            jitter_buffer: None,
        }
    }
}

pub struct RtcTrack {
    track_id: TrackId,
    track_config: TrackConfig,
    rtc_config: RtcTrackConfig,
    processor_chain: ProcessorChain,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    event_sender: Arc<Mutex<Option<EventSender>>>,
    media_ready_sent: Arc<AtomicBool>,
    rtp_timeout: Arc<RtpTimeoutMonitor>,
    cancel_token: CancellationToken,
    local_source: Option<Arc<SampleStreamSource>>,
    encoder: TrackCodec,
    ssrc: u32,
    payload_type: Option<u8>,
    pub peer_connection: Option<Arc<PeerConnection>>,
    next_rtp_timestamp: u32,
    next_rtp_sequence_number: u16,
    /// Wall-clock anchor of the media clock, created with the track.
    anchor: MediaAnchor,
    last_remote_sdp: Option<String>,
    /// Negotiated telephone-event payload types as `(pt, clock_rate)`.
    telephone_events: Vec<(u8, u32)>,
    /// Outbound telephone-event in progress, if any.
    dtmf_tx: Option<DtmfTxState>,
}

/// Incoming-audio watchdog. The event loop checks it once a second while
/// it is armed: answered, a timeout configured and the peer sending.
struct RtpTimeoutMonitor {
    epoch: tokio::time::Instant,
    /// Milliseconds since `epoch` of the last received audio sample.
    last_rx_ms: AtomicU64,
    state: tokio::sync::watch::Sender<RtpTimeoutState>,
}

#[derive(Clone, Copy, PartialEq)]
struct RtpTimeoutState {
    timeout: Option<Duration>,
    answered: bool,
    /// Remote SDP says the peer sends audio (not recvonly/inactive).
    peer_sending: bool,
}

impl RtpTimeoutMonitor {
    fn new(timeout: Option<Duration>) -> Self {
        Self {
            epoch: tokio::time::Instant::now(),
            last_rx_ms: AtomicU64::new(0),
            state: tokio::sync::watch::Sender::new(RtpTimeoutState {
                timeout,
                answered: false,
                peer_sending: true,
            }),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// `rtp_timeout`, when given, replaces the configured one.
    fn answer(&self, rtp_timeout: Option<Duration>) {
        self.update(|s| {
            s.answered = true;
            if rtp_timeout.is_some() {
                s.timeout = rtp_timeout;
            }
        });
    }

    fn update(&self, f: impl FnOnce(&mut RtpTimeoutState)) {
        self.state.send_if_modified(|state| {
            let before = *state;
            f(state);
            *state != before
        });
    }
}

impl RtpTimeoutState {
    fn armed(&self) -> Option<Duration> {
        self.timeout
            .filter(|t| !t.is_zero() && self.answered && self.peer_sending)
    }
}

/// Fixed point tying the RTP media clock to the wall clock: at `instant`
/// the clock was at `rtp_timestamp`, ticking at `clock_rate` (0 until the
/// first packet).
struct MediaAnchor {
    instant: Instant,
    rtp_timestamp: u32,
    clock_rate: u32,
}

/// RFC 4733 send state for the event currently on the wire.
struct DtmfTxState {
    event: u8,
    payload_type: u8,
    /// RTP timestamp of the event start; constant for all its packets.
    rtp_timestamp: u32,
    /// RTP clock of the audio codec, also used for the duration field.
    clock_rate: u32,
    duration_ms: u32,
    ended: bool,
    last_update: Instant,
}

/// Payload type of the 48kHz telephone-event offered alongside Opus.
const TELEPHONE_EVENT_48K_PAYLOAD_TYPE: u8 = 110;

/// Retransmissions of the final (E=1) packet, per RFC 4733 §2.5.1.4.
const DTMF_END_PACKET_COUNT: usize = 3;
/// Volume field of generated events (-10 dBm0).
const DTMF_VOLUME: u8 = 10;
/// An event with no update for this long is treated as ended.
const DTMF_STALE_TIMEOUT: Duration = Duration::from_millis(1000);
/// Re-anchor the media clock once wall clock runs this far ahead of it.
const MEDIA_GAP_THRESHOLD: Duration = Duration::from_millis(30);

impl RtcTrack {
    pub fn new(
        cancel_token: CancellationToken,
        id: TrackId,
        track_config: TrackConfig,
        rtc_config: RtcTrackConfig,
    ) -> Self {
        let processor_chain = ProcessorChain::new(track_config.samplerate);
        // RFC 3550 §5.1: the initial timestamp should be random.
        let initial_rtp_timestamp = rand::random();
        let rtp_timeout = Arc::new(RtpTimeoutMonitor::new(rtc_config.rtp_timeout));
        Self {
            track_id: id,
            track_config,
            rtc_config,
            processor_chain,
            packet_sender: Arc::new(Mutex::new(None)),
            event_sender: Arc::new(Mutex::new(None)),
            media_ready_sent: Arc::new(AtomicBool::new(false)),
            rtp_timeout,
            cancel_token,
            local_source: None,
            encoder: TrackCodec::new(),
            ssrc: 0,
            payload_type: None,
            peer_connection: None,
            next_rtp_timestamp: initial_rtp_timestamp,
            next_rtp_sequence_number: 0,
            anchor: MediaAnchor {
                instant: Instant::now(),
                rtp_timestamp: initial_rtp_timestamp,
                clock_rate: 0,
            },
            last_remote_sdp: None,
            telephone_events: Vec::new(),
            dtmf_tx: None,
        }
    }

    pub fn with_ssrc(mut self, ssrc: u32) -> Self {
        self.ssrc = ssrc;
        self
    }

    pub fn create_audio_track(
        _codec: CodecType,
        _stream_id: Option<String>,
    ) -> (Arc<SampleStreamSource>, Arc<SampleStreamTrack>) {
        let (source, track, _) = sample_track(rustrtc::media::MediaKind::Audio, 100);
        (Arc::new(source), track)
    }

    pub async fn local_description(&self) -> Result<String> {
        let pc = self
            .peer_connection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No PeerConnection"))?;
        let offer = pc.create_offer().await?;
        pc.set_local_description(offer.clone())?;
        Ok(offer.to_sdp_string())
    }

    pub async fn create(&mut self) -> Result<()> {
        if self.peer_connection.is_some() {
            return Ok(());
        }

        let mut config = RtcConfiguration::default();
        if self.ssrc != 0 {
            config.ssrc_start = self.ssrc;
        }
        config.transport_mode = self.rtc_config.mode.clone();

        if let Some(ice_servers) = &self.rtc_config.ice_servers {
            config.ice_servers = ice_servers.clone();
        }

        if let Some(external_ip) = &self.rtc_config.external_ip {
            config.external_ip = Some(external_ip.clone());
        }
        if let Some(bind_ip) = &self.rtc_config.bind_ip {
            config.bind_ip = Some(bind_ip.clone());
        }
        if let Some((rtp_start_port, rtp_end_port)) = self.rtc_config.rtp_port_range {
            config.rtp_start_port = Some(rtp_start_port);
            config.rtp_end_port = Some(rtp_end_port);
        }
        config.enable_ice_lite = self.rtc_config.enable_ice_lite.unwrap_or(false);
        config.enable_latching = self
            .rtc_config
            .enable_latching
            .unwrap_or_else(|| self.rtc_config.mode == TransportMode::Rtp);

        // Audio codecs in the configured order (rustrtc's default audio set
        // when none are configured), followed by one telephone-event per
        // clock rate in use, in order of first appearance: DTMF must share
        // the audio codec's clock, so it is derived rather than configured.
        let audio = if self.rtc_config.codecs.is_empty() {
            MediaCapabilities::default().audio
        } else {
            self.rtc_config
                .codecs
                .iter()
                .filter_map(|codec| match codec {
                    CodecType::PCMU => Some(AudioCapability::pcmu()),
                    CodecType::PCMA => Some(AudioCapability::pcma()),
                    CodecType::G722 => Some(AudioCapability::g722()),
                    CodecType::G729 => Some(AudioCapability::g729()),
                    CodecType::Opus => Some(AudioCapability::opus()),
                    CodecType::TelephoneEvent => None,
                })
                .collect()
        };
        let mut caps = MediaCapabilities::default();
        caps.audio.clear();
        let mut event_clock_rates = Vec::new();
        for cap in audio {
            if cap.codec_name.eq_ignore_ascii_case("telephone-event")
                || caps
                    .audio
                    .iter()
                    .any(|c| c.payload_type == cap.payload_type)
            {
                continue;
            }
            if !event_clock_rates.contains(&cap.clock_rate) {
                event_clock_rates.push(cap.clock_rate);
            }
            caps.audio.push(cap);
        }
        for clock_rate in event_clock_rates {
            let payload_type = match clock_rate {
                8000 => AudioCapability::telephone_event().payload_type,
                48000 => TELEPHONE_EVENT_48K_PAYLOAD_TYPE,
                _ => continue,
            };
            caps.audio.push(AudioCapability {
                payload_type,
                clock_rate,
                ..AudioCapability::telephone_event()
            });
        }
        config.media_capabilities = Some(caps);

        let peer_connection = Arc::new(PeerConnection::new(config));
        self.peer_connection = Some(peer_connection.clone());

        let default_codec = CodecType::G722;
        let codec = self.rtc_config.preferred_codec.unwrap_or(default_codec);

        let (source, track) = Self::create_audio_track(codec, Some(self.track_id.clone()));
        self.local_source = Some(source);

        let payload_type = self
            .rtc_config
            .payload_type
            .unwrap_or_else(|| codec.payload_type());

        self.payload_type = Some(payload_type);

        let params = RtpCodecParameters {
            clock_rate: codec.clock_rate(),
            channels: codec.channels() as u8,
            payload_type,
            ..Default::default()
        };

        peer_connection.add_track_with_stream_id(track, self.track_id.clone(), params)?;

        // Spawn Handler Logic
        self.spawn_handlers(
            peer_connection.clone(),
            self.track_id.clone(),
            self.processor_chain.clone(),
            payload_type,
            self.event_sender.clone(),
            self.media_ready_sent.clone(),
        );

        Ok(())
    }

    fn spawn_handlers(
        &self,
        pc: Arc<PeerConnection>,
        track_id: TrackId,
        processor_chain: ProcessorChain,
        default_payload_type: u8,
        event_sender: Arc<Mutex<Option<EventSender>>>,
        media_ready_sent: Arc<AtomicBool>,
    ) {
        let cancel_token = self.cancel_token.clone();
        let packet_sender = self.packet_sender.clone();
        let rtp_timeout = self.rtp_timeout.clone();
        let pc_event = pc.clone();
        let pc_stats = pc.clone();
        let pc_state = pc.clone();
        let track_id_log = track_id.clone();
        let is_rtp_media = matches!(
            self.rtc_config.mode,
            TransportMode::Rtp | TransportMode::Srtp
        );
        let is_webrtc = self.rtc_config.mode != TransportMode::Rtp;
        let playout_ptime = self
            .rtc_config
            .jitter_buffer
            .unwrap_or(self.rtc_config.mode == TransportMode::WebRtc)
            .then_some(self.track_config.ptime);

        crate::spawn(async move {
            info!(track_id=%track_id_log, "RtcTrack event/stats loop started");

            let mut events = futures::stream::unfold(pc_event, |pc| async move {
                pc.recv().await.map(|ev| (ev, pc))
            })
            .boxed();

            let mut state_rx = if is_webrtc {
                Some(pc_state.subscribe_peer_state())
            } else {
                None
            };

            let mut stats_interval = tokio::time::interval(Duration::from_secs(5));
            let mut timeout_interval = tokio::time::interval(Duration::from_secs(1));
            timeout_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut timeout_state = rtp_timeout.state.subscribe();
            let mut armed = timeout_state.borrow_and_update().armed();
            // Arming (or re-arming after hold) grants a full timeout.
            let mut armed_at_ms = rtp_timeout.now_ms();
            // `last_rx_ms` already reported, so each gap is reported once.
            let mut notified_rx_ms = None;
            let mut event_count = 0;
            let mut workers = FuturesUnordered::new();

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!(track_id=%track_id_log, "RtcTrack loop cancelled");
                        break;
                    }

                    Some(event) = events.next() => {
                        event_count += 1;
                        let event_type = match &event {
                            PeerConnectionEvent::Track(_) => "Track",
                            PeerConnectionEvent::DataChannel(_) => "DataChannel",
                        };
                        debug!(track_id=%track_id_log, "Received PeerConnectionEvent #{}: {}", event_count, event_type);

                        if let PeerConnectionEvent::Track(transceiver) = event {
                            if let Some(receiver) = transceiver.receiver() {
                                let track = receiver.track();
                                if is_rtp_media {
                                    let maybe_sender = event_sender.lock().await.clone();
                                    if let Some(sender) = maybe_sender {
                                        if media_ready_sent
                                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                            .is_ok()
                                        {
                                            let result = sender.send(SessionEvent::MediaReady {
                                                track_id: track_id_log.clone(),
                                                timestamp: crate::media::get_timestamp(),
                                            });
                                            if result.is_err() {
                                                media_ready_sent.store(false, Ordering::SeqCst);
                                            }
                                        }
                                    }
                                }
                                info!(track_id=%track_id_log, "New track received");

                                let (f1, f2) = Self::create_track_workers(
                                    track,
                                    packet_sender.clone(),
                                    track_id_log.clone(),
                                    processor_chain.clone(),
                                    default_payload_type,
                                    rtp_timeout.clone(),
                                    playout_ptime,
                                );
                                workers.push(f1);
                                workers.push(f2);
                            }
                        }
                    }

                    _ = workers.next(), if !workers.is_empty() => {}

                    Ok(()) = timeout_state.changed() => {
                        let next = timeout_state.borrow_and_update().armed();
                        if next != armed {
                            armed = next;
                            armed_at_ms = rtp_timeout.now_ms();
                            notified_rx_ms = None;
                        }
                    }

                    _ = timeout_interval.tick(), if armed.is_some() => {
                        let timeout = armed.unwrap_or_default();
                        let last_rx_ms = rtp_timeout.last_rx_ms.load(Ordering::Relaxed);
                        let idle_ms = rtp_timeout
                            .now_ms()
                            .saturating_sub(last_rx_ms.max(armed_at_ms));
                        if notified_rx_ms != Some(last_rx_ms)
                            && idle_ms >= timeout.as_millis() as u64
                        {
                            if let Some(sender) = event_sender.lock().await.as_ref() {
                                let event = SessionEvent::RtpTimeout {
                                    track_id: track_id_log.clone(),
                                    timestamp: crate::media::get_timestamp(),
                                    timeout: timeout.as_secs(),
                                };
                                if sender.send(event).is_ok() {
                                    notified_rx_ms = Some(last_rx_ms);
                                }
                            }
                        }
                    }

                    _ = stats_interval.tick() => {
                        match pc_stats.get_stats().await {
                            Ok(stats) => {
                                // Empty reports are normal for tracks with no
                                // RTP flowing yet (freshly created / stopped).
                                if stats.entries.is_empty() {
                                    debug!(track_id=%track_id_log, "RTCP Stats: empty report");
                                } else {
                                    info!(track_id=%track_id_log, %stats, "RTCP Stats");
                                }
                            }
                            Err(e) => {
                                debug!(track_id=%track_id_log, "Failed to get stats: {:?}", e);
                            }
                        }
                    }

                    // Handle state changes for transports that expose them.
                    res = async {
                        if let Some(rx) = state_rx.as_mut() {
                            rx.changed().await
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        if res.is_ok() {
                            if let Some(rx) = state_rx.as_ref() {
                                let s = *rx.borrow();
                                debug!(track_id=%track_id_log, "peer connection state changed: {:?}", s);
                                match s {
                                    PeerConnectionState::Disconnected
                                    | PeerConnectionState::Closed
                                    | PeerConnectionState::Failed => {
                                        info!(
                                            track_id = %track_id_log,
                                            "peer connection is {:?}, try to close", s
                                        );
                                        cancel_token.cancel();
                                        pc_state.close();
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            debug!(track_id=%track_id_log, "RtcTrack event/stats loop ended, total events: {}", event_count);
        });
    }

    fn create_track_workers(
        track: Arc<SampleStreamTrack>,
        packet_sender_arc: Arc<Mutex<Option<TrackPacketSender>>>,
        track_id: TrackId,
        processor_chain: ProcessorChain,
        default_payload_type: u8,
        rtp_timeout: Arc<RtpTimeoutMonitor>,
        playout_ptime: Option<Duration>,
    ) -> (
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Playout<RtcAudioFrame>>();

        let recv_fut = Self::run_receiving_worker(track, tx, track_id.clone(), rtp_timeout);
        let proc_fut = match playout_ptime {
            Some(ptime) => {
                let (playout_tx, playout_rx) = tokio::sync::mpsc::unbounded_channel();
                let playout = Self::run_playout_worker(rx, playout_tx, track_id.clone(), ptime);
                let processing = Self::run_processing_worker(
                    playout_rx,
                    track_id,
                    packet_sender_arc,
                    processor_chain,
                    default_payload_type,
                );
                async move {
                    futures::join!(playout, processing);
                }
                .boxed()
            }
            None => Self::run_processing_worker(
                rx,
                track_id,
                packet_sender_arc,
                processor_chain,
                default_payload_type,
            )
            .boxed(),
        };

        (proc_fut, recv_fut.boxed())
    }

    /// Releases received frames one per packet time, in sequence order,
    /// reporting the missing ones as `Playout::Lost` (see `media::jitter`).
    async fn run_playout_worker(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<Playout<RtcAudioFrame>>,
        tx: tokio::sync::mpsc::UnboundedSender<Playout<RtcAudioFrame>>,
        track_id: TrackId,
        mut ptime: Duration,
    ) {
        let mut buffer = PlayoutBuffer::new(PlayoutConfig::default());
        let mut ticker = tokio::time::interval(ptime);
        // the sender's last frame, to measure its packet time from
        let mut last: Option<(u16, u32)> = None;
        // a packet time other than ours, and how many frames in a row had it
        let mut other_ptime: (Duration, u32) = (ptime, 0);
        loop {
            tokio::select! {
                item = rx.recv() => {
                    let Some(Playout::Frame(frame)) = item else { break };
                    let Some(seq) = frame.sequence_number else {
                        // nothing to order it by
                        if tx.send(Playout::Frame(frame)).is_err() {
                            break;
                        }
                        continue;
                    };
                    // follow the sender's packet time, so the buffer neither
                    // drains nor fills against a ptime other than ours
                    if let Some((last_seq, last_ts)) = last
                        && seq == last_seq.wrapping_add(1)
                        && frame.clock_rate > 0
                    {
                        let ticks = frame.rtp_timestamp.wrapping_sub(last_ts) as u64;
                        let measured = Duration::from_micros(ticks * 1_000_000 / frame.clock_rate as u64);
                        if measured == ptime
                            || measured < Duration::from_millis(10)
                            || measured > Duration::from_millis(60)
                        {
                            other_ptime = (ptime, 0);
                        } else if measured == other_ptime.0 {
                            other_ptime.1 += 1;
                        } else {
                            other_ptime = (measured, 1);
                        }
                        if other_ptime.1 >= 5 {
                            info!(track_id=%track_id, ?measured, "playout packet time changed");
                            ptime = measured;
                            ticker = tokio::time::interval(ptime);
                            other_ptime = (ptime, 0);
                        }
                    }
                    last = Some((seq, frame.rtp_timestamp));
                    if let Some(packet) = &frame.raw_packet {
                        buffer.set_source(packet.header.ssrc);
                    }
                    buffer.push(seq, frame);
                }
                _ = ticker.tick() => {
                    let out = match buffer.tick() {
                        Playout::Idle => continue,
                        out => out,
                    };
                    if tx.send(out).is_err() {
                        break;
                    }
                }
            }
        }
        let stats = buffer.stats();
        info!(
            track_id=%track_id,
            received = stats.received,
            played = stats.played,
            lost = stats.lost,
            underruns = stats.underruns,
            late = stats.late,
            duplicates = stats.duplicates,
            dropped = stats.dropped,
            resets = stats.resets,
            "RtcTrack playout stopped"
        );
    }

    async fn run_processing_worker(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<Playout<RtcAudioFrame>>,
        track_id: TrackId,
        packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
        mut processor_chain: ProcessorChain,
        default_payload_type: u8,
    ) {
        info!(track_id=%track_id, "RtcTrack processing worker started");
        let mut concealer = Concealer::new();
        while let Some(item) = rx.recv().await {
            let frame = match item {
                Playout::Frame(frame) => frame,
                Playout::Lost => {
                    Self::conceal_frame(&mut concealer, &track_id, &packet_sender).await;
                    continue;
                }
                Playout::Idle => continue,
            };
            let res = std::panic::AssertUnwindSafe(Self::process_audio_frame(
                frame,
                &track_id,
                &packet_sender,
                &mut processor_chain,
                default_payload_type,
                &mut concealer,
            ))
            .catch_unwind()
            .await;

            if let Err(cause) = res {
                let msg = if let Some(s) = cause.downcast_ref::<&str>() {
                    *s
                } else if let Some(s) = cause.downcast_ref::<String>() {
                    &s[..]
                } else {
                    "Unknown panic"
                };
                tracing::error!(track_id=%track_id, "RtcTrack processing worker PANIC: {}", msg);
                break;
            }
        }
        info!(track_id=%track_id, "RtcTrack processing worker stopped");
    }

    /// Plays a concealment frame in place of a lost one. Only possible once
    /// the chain decodes this track to PCM; undecoded RTP passes through
    /// with the gap.
    async fn conceal_frame(
        concealer: &mut Concealer,
        track_id: &TrackId,
        packet_sender: &Arc<Mutex<Option<TrackPacketSender>>>,
    ) {
        let Some((samples, sample_rate)) = concealer.conceal() else {
            return;
        };
        let packet_sender = packet_sender.lock().await;
        if let Some(sender) = packet_sender.as_ref() {
            sender
                .send(AudioFrame {
                    track_id: track_id.clone(),
                    samples: crate::media::Samples::PCM { samples },
                    timestamp: crate::media::get_timestamp(),
                    sample_rate,
                    channels: 1,
                    ..Default::default()
                })
                .ok();
        }
    }

    async fn run_receiving_worker(
        track: Arc<SampleStreamTrack>,
        tx: tokio::sync::mpsc::UnboundedSender<Playout<RtcAudioFrame>>,
        track_id: TrackId,
        rtp_timeout: Arc<RtpTimeoutMonitor>,
    ) {
        let mut samples =
            futures::stream::unfold(
                track,
                |t| async move { t.recv().await.ok().map(|s| (s, t)) },
            )
            .boxed();

        while let Some(sample) = samples.next().await {
            if let rustrtc::media::frame::MediaSample::Audio(frame) = sample {
                rtp_timeout
                    .last_rx_ms
                    .store(rtp_timeout.now_ms(), Ordering::Relaxed);
                if let Err(_) = tx.send(Playout::Frame(frame)) {
                    break;
                }
            } else {
                debug!(track_id=%track_id, "Received non-audio sample");
            }
        }
        info!(track_id=%track_id, "RtcTrack receiving worker stopped");
    }

    async fn process_audio_frame(
        frame: rustrtc::media::frame::AudioFrame,
        track_id: &TrackId,
        packet_sender: &Arc<Mutex<Option<TrackPacketSender>>>,
        processor_chain: &mut ProcessorChain,
        default_payload_type: u8,
        concealer: &mut Concealer,
    ) {
        let packet_sender = packet_sender.lock().await;
        if let Some(sender) = packet_sender.as_ref() {
            let payload_type = frame.payload_type.unwrap_or(default_payload_type);
            let src_codec = match processor_chain.codec.get_codec_for_pt(payload_type) {
                Some(c) => c,
                None => {
                    debug!(track_id=%track_id, "Unknown payload type {}, skipping frame", payload_type);
                    return;
                }
            };

            let mut af = AudioFrame {
                track_id: track_id.clone(),
                samples: crate::media::Samples::RTP {
                    payload_type,
                    payload: frame.data.to_vec(),
                    sequence_number: frame.sequence_number.unwrap_or(0),
                },
                timestamp: crate::media::get_timestamp(),
                sample_rate: src_codec.samplerate(),
                channels: src_codec.channels(),
                ..Default::default()
            };
            if let Err(e) = processor_chain.process_frame(&mut af) {
                debug!(track_id=%track_id, "processor_chain process_frame error: {:?}", e);
            }
            if let crate::media::Samples::PCM { samples } = &mut af.samples
                && af.channels == 1
            {
                concealer.on_frame(samples, af.sample_rate);
            }

            sender.send(af).ok();
        }
    }

    pub fn parse_sdp_payload_types(&mut self, sdp_type: SdpType, sdp_str: &str) -> Result<()> {
        use crate::media::negotiate::parse_rtpmap;
        let sdp = rustrtc::SessionDescription::parse(sdp_type, sdp_str)?;

        if let Some(media) = sdp
            .media_sections
            .iter()
            .find(|m| m.kind == MediaKind::Audio)
        {
            let mut telephone_events = Vec::new();
            for attr in &media.attributes {
                if attr.key == "rtpmap" {
                    if let Some(value) = &attr.value {
                        if let Ok((pt, codec, clock_rate, _)) = parse_rtpmap(value) {
                            if codec == CodecType::TelephoneEvent {
                                telephone_events.push((pt, clock_rate));
                            }
                            self.encoder.set_payload_type(pt, codec.clone());
                            self.processor_chain.codec.set_payload_type(pt, codec);
                        }
                    }
                }
            }

            self.telephone_events = telephone_events;

            // Negotiate primary audio codec
            let mut negotiated = None;

            // When parsing an answer, prefer our configured codec order among accepted codecs.
            // Offer parsing is provisional; the final outgoing PT is set from the answer.
            if sdp_type == rustrtc::sdp::SdpType::Answer && !self.rtc_config.codecs.is_empty() {
                for preferred_codec in &self.rtc_config.codecs {
                    if *preferred_codec == CodecType::TelephoneEvent {
                        continue;
                    }
                    for fmt in &media.formats {
                        if let Ok(pt) = fmt.parse::<u8>() {
                            let codec = self.encoder.get_codec_for_pt(pt);
                            if let Some(c) = codec {
                                if c == *preferred_codec {
                                    negotiated = Some((pt, c));
                                    break;
                                }
                            }
                        }
                    }
                    if negotiated.is_some() {
                        break;
                    }
                }
            }

            // Fallback: use the first codec in the SDP (matches offerer's preference if we are answerer)
            if negotiated.is_none() {
                for fmt in &media.formats {
                    if let Ok(pt) = fmt.parse::<u8>() {
                        let codec = self.encoder.get_codec_for_pt(pt);
                        if let Some(codec) = codec {
                            if codec != CodecType::TelephoneEvent {
                                negotiated = Some((pt, codec));
                                break;
                            }
                        }
                    }
                }
            }

            if let Some((pt, codec)) = negotiated {
                info!(track_id=%self.track_id, "Negotiated primary audio PT {} ({:?})", pt, codec);
                self.payload_type = Some(pt);
            }
        }
        Ok(())
    }

    fn normalize_sdp(sdp: &str) -> String {
        sdp.lines()
            .map(|line| {
                if line.starts_with("o=") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 3 {
                        return format!("o= {} {}", parts[1], parts[2]);
                    }
                }
                line.to_string()
            })
            .filter(|line| {
                !line.starts_with("t=") &&  // timing line can vary
                !line.starts_with("a=ssrc:") &&  // SSRC attributes (but SSRC change shows in o= version)
                !line.starts_with("a=msid:") &&  // media stream ID
                !line.trim().is_empty()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Track whether the remote side will send audio, from its SDP.
    fn update_peer_sending(&self, sdp: &rustrtc::SessionDescription) {
        let peer_sending = sdp.media_sections.iter().any(|media| {
            media.kind == MediaKind::Audio
                && media.port != 0
                && matches!(
                    media.direction,
                    rustrtc::sdp::Direction::SendRecv | rustrtc::sdp::Direction::SendOnly
                )
        });
        self.rtp_timeout.update(|s| s.peer_sending = peer_sending);
    }

    /// A final (non-provisional) remote answer means the peer accepted our
    /// offer, e.g. the 200 OK of an outbound INVITE.
    fn on_remote_answer(&self, sdp_type: rustrtc::SdpType) {
        if sdp_type == rustrtc::SdpType::Answer {
            self.rtp_timeout.update(|s| s.answered = true);
        }
    }

    async fn update_remote_description_internal(
        &mut self,
        answer: &String,
        force_update: bool,
        sdp_type: rustrtc::SdpType,
    ) -> Result<()> {
        info!(
            track_id=%self.track_id,
            "update_remote_description_internal called. force={}, last_sdp_is_some={}, mode={:?}, sdp_type={:?}",
            force_update,
            self.last_remote_sdp.is_some(),
            self.rtc_config.mode,
            sdp_type
        );

        if let Some(pc) = &self.peer_connection {
            if !force_update {
                if let Some(ref last_sdp) = self.last_remote_sdp {
                    if Self::normalize_sdp(last_sdp) == Self::normalize_sdp(answer) {
                        debug!(track_id=%self.track_id, "SDP unchanged, skipping update_remote_description");
                        self.on_remote_answer(sdp_type);
                        return Ok(());
                    }
                }
            } else {
                debug!(track_id=%self.track_id, "Force update requested, skipping SDP comparison");
            }

            let _is_first_remote_sdp = self.last_remote_sdp.is_none();

            let sdp_obj = rustrtc::SessionDescription::parse(sdp_type, answer)?;
            match pc.set_remote_description(sdp_obj.clone()).await {
                Ok(_) => {
                    debug!(track_id=%self.track_id, "set_remote_description succeeded");
                    self.last_remote_sdp = Some(answer.clone());
                }
                Err(e) => {
                    if self.rtc_config.mode == TransportMode::Rtp {
                        info!(track_id=%self.track_id, "set_remote_description failed ({}), attempting to re-sync state for SIP update", e);

                        if let Some(current_local) = pc.local_description() {
                            let sdp = current_local.to_sdp_string();
                            for line in sdp.lines() {
                                if line.starts_with("a=ssrc:") {
                                    info!(track_id=%self.track_id, "SSRC before re-sync: {}", line);
                                }
                            }
                        }

                        let offer = pc.create_offer().await?;

                        let sdp = offer.to_sdp_string();
                        for line in sdp.lines() {
                            if line.starts_with("a=ssrc:") {
                                info!(track_id=%self.track_id, "SSRC in new offer (re-sync): {}", line);
                            }
                        }

                        pc.set_local_description(offer)?;
                        pc.set_remote_description(sdp_obj.clone()).await?;
                        self.last_remote_sdp = Some(answer.clone());
                        info!(track_id=%self.track_id, "successfully re-synced WebRTC state for SIP update");
                    } else {
                        return Err(e.into());
                    }
                }
            }

            self.update_peer_sending(&sdp_obj);
            self.on_remote_answer(sdp_type);

            // Track events will be handled by the event loop after SSRC latching

            // Extract negotiated payload types from SDP string
            self.parse_sdp_payload_types(sdp_type, answer)?;
        }
        Ok(())
    }
}

#[async_trait]
impl Track for RtcTrack {
    fn ssrc(&self) -> u32 {
        self.ssrc
    }
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.track_config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }

    async fn handshake(&mut self, offer: String, _: Option<Duration>) -> Result<String> {
        info!(track_id=%self.track_id, "rtc handshake start");
        self.create().await?;

        let pc = self.peer_connection.clone().ok_or_else(|| {
            anyhow::anyhow!("No PeerConnection available for track {}", self.track_id)
        })?;

        debug!(track_id=%self.track_id, "Before set_remote_description: transceivers count = {}", pc.get_transceivers().len());
        for (i, t) in pc.get_transceivers().iter().enumerate() {
            debug!(track_id=%self.track_id, "  Transceiver #{}: kind={:?}, mid={:?}, direction={:?}",
                i, t.kind(), t.mid(), t.direction());
        }

        let sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer)?;
        pc.set_remote_description(sdp.clone()).await?;
        self.update_peer_sending(&sdp);

        debug!(track_id=%self.track_id, "After set_remote_description: transceivers count = {}", pc.get_transceivers().len());
        for (i, t) in pc.get_transceivers().iter().enumerate() {
            debug!(track_id=%self.track_id, "  Transceiver #{}: kind={:?}, mid={:?}, direction={:?}, has_receiver={}",
                i, t.kind(), t.mid(), t.direction(), t.receiver().is_some());
        }

        // For RTP mode: Wait for PeerConnectionEvent::Track after SSRC latching completes
        // For WebRTC mode: The event loop will handle Track events
        info!(track_id=%self.track_id, "Waiting for Track events (SSRC latching for RTP mode)");

        self.parse_sdp_payload_types(rustrtc::SdpType::Offer, &offer)?;

        let mut answer = pc.create_answer().await?;
        crate::media::negotiate::intersect_answer(&sdp, &mut answer);
        self.parse_sdp_payload_types(rustrtc::SdpType::Answer, &answer.to_sdp_string())?;

        pc.set_local_description(answer.clone())?;

        if self.rtc_config.mode != TransportMode::Rtp {
            pc.wait_for_gathering_complete().await;
        }

        let final_answer = pc
            .local_description()
            .ok_or(anyhow::anyhow!("No local description"))?;

        Ok(final_answer.to_sdp_string())
    }

    fn on_answered(&self, rtp_timeout: Option<Duration>) {
        self.rtp_timeout.answer(rtp_timeout);
    }

    async fn update_remote_description(&mut self, answer: &String) -> Result<()> {
        self.update_remote_description_internal(answer, false, rustrtc::SdpType::Answer)
            .await
    }

    async fn update_remote_description_force(&mut self, answer: &String) -> Result<()> {
        self.update_remote_description_internal(answer, true, rustrtc::SdpType::Answer)
            .await
    }

    async fn update_remote_description_provisional(&mut self, answer: &String) -> Result<()> {
        // SIP 183 early media: apply as a provisional answer so signaling
        // state stays in HaveLocalOffer, leaving room for the real 200 OK
        // answer to complete negotiation. Tagging this as a full Answer (as
        // the final answer does) would move state to Stable early, so the
        // real answer would then be rejected as an invalid re-application
        // and only recover via the SDP-mismatch re-sync fallback below —
        // losing/disrupting the media path for the ringing window.
        self.update_remote_description_internal(answer, false, rustrtc::SdpType::Pranswer)
            .await
    }

    async fn start(
        &mut self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        *self.packet_sender.lock().await = Some(packet_sender.clone());
        *self.event_sender.lock().await = Some(event_sender.clone());
        let token_clone = self.cancel_token.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.track_id.clone();
        let ssrc = self.ssrc;

        if self.rtc_config.mode != TransportMode::Rtp {
            let start_time = crate::media::get_timestamp();
            crate::spawn(async move {
                token_clone.cancelled().await;
                let _ = event_sender_clone.send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::media::get_timestamp(),
                    duration: crate::media::get_timestamp() - start_time,
                    ssrc,
                    play_id: None,
                    auto_hangup: None,
                });
            });
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        if let Some(pc) = &self.peer_connection {
            pc.close();
        }
        Ok(())
    }

    async fn send_packet(&mut self, packet: &AudioFrame) -> Result<()> {
        let Some(source) = self.local_source.clone() else {
            return Ok(());
        };

        match &packet.samples {
            crate::media::Samples::Dtmf {
                event,
                duration_ms,
                end,
            } => {
                self.send_dtmf_update(&source, *event, *duration_ms, *end);
            }
            _ if self.dtmf_event_active() => {
                // Audio is muted while a telephone-event is in progress.
            }
            crate::media::Samples::PCM { samples } => {
                let payload_type = self.get_payload_type();
                let (_, encoded) = self.encoder.encode(payload_type, packet.clone());
                let target_codec = self
                    .encoder
                    .get_codec_for_pt(payload_type)
                    .ok_or_else(|| anyhow::anyhow!("Invalid codec type: {}", payload_type))?;
                if !encoded.is_empty() {
                    let clock_rate = target_codec.clock_rate();
                    let frames = samples.len() as u64 / packet.channels.max(1) as u64;
                    let duration = if packet.sample_rate > 0 {
                        Duration::from_nanos(frames * 1_000_000_000 / packet.sample_rate as u64)
                    } else {
                        self.track_config.ptime
                    };
                    let (rtp_timestamp, marker) = self.next_media_timestamp(clock_rate, duration);
                    let sequence_number = self.next_sequence_number();

                    let frame = RtcAudioFrame {
                        data: Bytes::from(encoded),
                        clock_rate,
                        payload_type: Some(payload_type),
                        sequence_number: Some(sequence_number),
                        rtp_timestamp,
                        marker,
                        ..Default::default()
                    };
                    source.try_send_audio(frame).ok();
                }
            }
            crate::media::Samples::RTP {
                payload,
                payload_type,
                ..
            } => {
                let target_codec = self
                    .encoder
                    .get_codec_for_pt(*payload_type)
                    .ok_or_else(|| anyhow::anyhow!("Invalid codec type: {}", payload_type))?;
                let clock_rate = target_codec.clock_rate();
                let (rtp_timestamp, marker) =
                    self.next_media_timestamp(clock_rate, self.track_config.ptime);
                let sequence_number = self.next_sequence_number();

                let frame = RtcAudioFrame {
                    data: Bytes::from(payload.clone()),
                    clock_rate,
                    payload_type: Some(*payload_type),
                    sequence_number: Some(sequence_number),
                    rtp_timestamp,
                    marker,
                    ..Default::default()
                };
                source.try_send_audio(frame).ok();
            }
            _ => {}
        }
        Ok(())
    }

    fn add_ice_candidate(
        &self,
        candidate: &str,
        // single audio m-line per track, so unused here
        _sdp_mid: Option<&str>,
        _sdp_mline_index: Option<u32>,
    ) -> Result<()> {
        let pc = self.peer_connection.as_ref().ok_or_else(|| {
            anyhow::anyhow!("No PeerConnection available for track {}", self.track_id)
        })?;
        pc.add_ice_candidate(IceCandidate::from_sdp(candidate)?)?;
        Ok(())
    }
}

impl RtcTrack {
    fn next_sequence_number(&mut self) -> u16 {
        let seq = self.next_rtp_sequence_number;
        self.next_rtp_sequence_number = seq.wrapping_add(1);
        seq
    }

    /// Timestamp and marker for an outgoing media packet carrying
    /// `duration` of audio. The clock advances by exactly that duration in
    /// `clock_rate` ticks, after syncing to the wall clock; the marker is set
    /// on the packet where the clock jumped.
    fn next_media_timestamp(&mut self, clock_rate: u32, duration: Duration) -> (u32, bool) {
        let marker = self.sync_media_clock(clock_rate);
        let rtp_timestamp = self.next_rtp_timestamp;
        self.next_rtp_timestamp =
            rtp_timestamp.wrapping_add(duration_to_rtp_ticks(duration, clock_rate));
        (rtp_timestamp, marker)
    }

    /// Sync the media clock to the wall clock. The wall-clock position is
    /// the anchor timestamp plus the time elapsed since the anchor instant;
    /// when it is ahead of the media clock by more than
    /// `MEDIA_GAP_THRESHOLD` (a gap in the outgoing audio, or accumulated
    /// drift), jump to it. Returns whether it jumped. The clock never moves
    /// backwards.
    fn sync_media_clock(&mut self, clock_rate: u32) -> bool {
        let elapsed = duration_to_rtp_ticks(self.anchor.instant.elapsed(), clock_rate);
        if self.anchor.clock_rate != clock_rate {
            // First packet or codec change: keep the instant and re-express
            // the anchor in this clock so the media clock continues from here.
            self.anchor.rtp_timestamp = self.next_rtp_timestamp.wrapping_sub(elapsed);
            self.anchor.clock_rate = clock_rate;
            return false;
        }
        let wall_timestamp = self.anchor.rtp_timestamp.wrapping_add(elapsed);
        let ahead = wall_timestamp.wrapping_sub(self.next_rtp_timestamp) as i32;
        if ahead <= duration_to_rtp_ticks(MEDIA_GAP_THRESHOLD, clock_rate) as i32 {
            return false;
        }
        self.next_rtp_timestamp = wall_timestamp;
        true
    }

    /// Whether a telephone-event is on the wire, muting regular audio.
    /// Events whose source stopped updating without an end are closed here.
    fn dtmf_event_active(&mut self) -> bool {
        let stale = match &self.dtmf_tx {
            Some(state) if !state.ended => state.last_update.elapsed() > DTMF_STALE_TIMEOUT,
            _ => return false,
        };
        if stale {
            debug!(track_id=%self.track_id, "telephone-event timed out without end");
            self.finish_dtmf_event();
            return false;
        }
        true
    }

    /// The negotiated telephone-event payload type sharing the audio clock.
    fn telephone_event_payload_type(&self, clock_rate: u32) -> Option<u8> {
        self.telephone_events
            .iter()
            .find(|(_, rate)| *rate == clock_rate)
            .map(|(pt, _)| *pt)
    }

    /// Map a transport-agnostic DTMF update onto RFC 4733 packets: a new
    /// event takes the current media timestamp and sets the marker bit, every
    /// update of the same event reuses that timestamp with a growing
    /// duration, and the end update is sent `DTMF_END_PACKET_COUNT` times.
    fn send_dtmf_update(
        &mut self,
        source: &SampleStreamSource,
        event: u8,
        duration_ms: u32,
        end: bool,
    ) {
        let in_progress = matches!(
            &self.dtmf_tx,
            Some(state) if state.event == event && !state.ended
        );

        let marker = if in_progress {
            false
        } else {
            if self.dtmf_event_active() {
                // A different event started before the previous one ended.
                self.finish_dtmf_event();
            }
            let clock_rate = self
                .encoder
                .get_codec_for_pt(self.get_payload_type())
                .map(|codec| codec.clock_rate())
                .unwrap_or(8000);
            let Some(payload_type) = self.telephone_event_payload_type(clock_rate) else {
                debug!(track_id=%self.track_id, event, clock_rate, "no telephone-event negotiated at the audio clock, dropping DTMF");
                return;
            };
            self.sync_media_clock(clock_rate);
            self.dtmf_tx = Some(DtmfTxState {
                event,
                payload_type,
                rtp_timestamp: self.next_rtp_timestamp,
                clock_rate,
                duration_ms: 0,
                ended: false,
                last_update: Instant::now(),
            });
            true
        };

        let Some(state) = self.dtmf_tx.as_mut() else {
            return;
        };
        state.duration_ms = duration_ms;
        state.last_update = Instant::now();
        let duration =
            duration_to_rtp_ticks(Duration::from_millis(duration_ms as u64), state.clock_rate)
                .min(u16::MAX as u32) as u16;
        let payload = Bytes::from(encode_event_payload(event, end, DTMF_VOLUME, duration).to_vec());
        let (payload_type, rtp_timestamp, clock_rate) =
            (state.payload_type, state.rtp_timestamp, state.clock_rate);

        let copies = if end { DTMF_END_PACKET_COUNT } else { 1 };
        for i in 0..copies {
            let frame = RtcAudioFrame {
                data: payload.clone(),
                clock_rate,
                payload_type: Some(payload_type),
                sequence_number: Some(self.next_sequence_number()),
                rtp_timestamp,
                marker: marker && i == 0,
                ..Default::default()
            };
            source.try_send_audio(frame).ok();
        }

        if end {
            self.finish_dtmf_event();
        }
    }

    /// Close the current event and resume the media clock after it.
    fn finish_dtmf_event(&mut self) {
        let Some(state) = self.dtmf_tx.as_mut() else {
            return;
        };
        if state.ended {
            return;
        }
        state.ended = true;
        let duration = Duration::from_millis(state.duration_ms as u64);
        self.next_rtp_timestamp = state
            .rtp_timestamp
            .wrapping_add(duration_to_rtp_ticks(duration, state.clock_rate));
    }

    fn get_payload_type(&self) -> u8 {
        if let Some(pt) = self.payload_type {
            return pt;
        }

        self.rtc_config.payload_type.unwrap_or_else(|| {
            match self.rtc_config.preferred_codec.unwrap_or(CodecType::G722) {
                CodecType::PCMU => 0,
                CodecType::PCMA => 8,
                CodecType::Opus => 111,
                CodecType::G722 => 9,
                CodecType::G729 => 18,
                _ => 111,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::track::TrackConfig;

    #[test]
    fn test_parse_sdp_payload_types() {
        let track_id = "test-track".to_string();
        let cancel_token = CancellationToken::new();
        let mut track = RtcTrack::new(
            cancel_token,
            track_id,
            TrackConfig::default(),
            RtcTrackConfig::default(),
        );

        // Case 1: Multiple audio codecs, telephone-event at the end. Primary should be PCMA (8)
        let sdp1 = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 8 0 101\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
        track
            .parse_sdp_payload_types(rustrtc::SdpType::Offer, sdp1)
            .expect("parse offer");
        assert_eq!(track.get_payload_type(), 8);

        // Case 2: telephone-event at the beginning, should skip it and pick PCMU (0)
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.preferred_codec = Some(CodecType::PCMU);
        let mut track2 = RtcTrack::new(
            CancellationToken::new(),
            "test-track-2".to_string(),
            TrackConfig::default(),
            rtc_config,
        );

        let sdp2 = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 101 0 8\r\na=rtpmap:101 telephone-event/8000\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\n";
        track2
            .parse_sdp_payload_types(rustrtc::SdpType::Offer, sdp2)
            .expect("parse offer");
        assert_eq!(track2.get_payload_type(), 0);

        // Case 3: Opus with dynamic payload type 111
        let sdp3 = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 111 101\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:101 telephone-event/8000\r\n";
        track
            .parse_sdp_payload_types(rustrtc::SdpType::Offer, sdp3)
            .expect("parse offer");
        assert_eq!(track.get_payload_type(), 111);

        // Case 4: Linphone can offer G729 first, but the final answer decides
        // the outgoing payload type.
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.preferred_codec = Some(CodecType::PCMU);
        rtc_config.codecs = vec![CodecType::PCMU, CodecType::PCMA];
        let mut track4 = RtcTrack::new(
            CancellationToken::new(),
            "test-track-4".to_string(),
            TrackConfig::default(),
            rtc_config,
        );

        let sdp4 = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 18 0 101\r\na=fmtp:18 annexb=yes\r\na=rtpmap:101 telephone-event/8000\r\n";
        track4
            .parse_sdp_payload_types(rustrtc::SdpType::Offer, sdp4)
            .expect("parse offer");
        assert_eq!(track4.get_payload_type(), 18);

        let answer4 = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        track4
            .parse_sdp_payload_types(rustrtc::SdpType::Answer, answer4)
            .expect("parse answer");
        assert_eq!(track4.get_payload_type(), 0);
    }

    #[tokio::test]
    async fn test_rtp_mode_handshake_spawns_handler() {
        use rustrtc::TransportMode;

        let track_id = "test-track-sip".to_string();
        let cancel = CancellationToken::new();
        let track_config = TrackConfig::default();
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = TransportMode::Rtp;
        rtc_config.preferred_codec = Some(CodecType::PCMU);
        rtc_config.codecs = vec![CodecType::PCMU, CodecType::TelephoneEvent];

        let mut track = RtcTrack::new(cancel, track_id, track_config, rtc_config);

        // Standard SIP/SDP offer
        let offer = "v=0\r\n\
o=- 123456 123456 IN IP4 172.0.0.1\r\n\
s=-\r\n\
c=IN IP4 172.0.0.1\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=sendrecv\r\n";

        // This should not panic and should set up the transceiver
        let res = track.handshake(offer.to_string(), None).await;
        assert!(res.is_ok(), "handshake failed: {res:?}");

        // We can inspect the PeerConnection to ensure it has a transceiver with a receiver
        if let Some(pc) = &track.peer_connection {
            let transceivers = pc.get_transceivers();
            // With the fix, we expect the logic to have iterated these transceivers.
            // In RTP/Receive mode, we should have 1 transceiver with a receiver.
            assert_eq!(transceivers.len(), 1);
            assert!(transceivers[0].receiver().is_some());
        } else {
            panic!("PeerConnection not initialized");
        }
    }

    fn webrtc_offer(ufrag: &str, pwd: &str) -> String {
        format!(
            "v=0\r\n\
o=- 123456 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 0\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:{ufrag}\r\n\
a=ice-pwd:{pwd}\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:0 PCMU/8000\r\n"
        )
    }

    fn sdp_attr(sdp: &str, name: &str) -> String {
        let prefix = format!("a={name}:");
        sdp.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("no {name} in sdp: {sdp}"))
            .trim()
            .to_string()
    }

    /// A second offer with new ICE credentials (browser `restartIce()`) must be
    /// answered on the same PeerConnection, with the answer carrying fresh
    /// local ICE credentials while DTLS identity stays the same.
    #[tokio::test]
    async fn test_webrtc_reoffer_restarts_ice_on_same_peer_connection() {
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = rustrtc::TransportMode::WebRtc;
        rtc_config.enable_ice_lite = Some(true);
        rtc_config.codecs = vec![CodecType::PCMU];

        let mut track = RtcTrack::new(
            CancellationToken::new(),
            "test-track-webrtc".to_string(),
            TrackConfig::default(),
            rtc_config,
        );

        let answer_1 = track
            .handshake(webrtc_offer("ufr1", "pwd1pwd1pwd1pwd1pwd1pwd1"), None)
            .await
            .expect("initial handshake");
        let pc_1 = track.peer_connection.clone().expect("peer connection");

        let answer_2 = track
            .handshake(webrtc_offer("ufr2", "pwd2pwd2pwd2pwd2pwd2pwd2"), None)
            .await
            .expect("re-offer handshake");
        let pc_2 = track.peer_connection.clone().expect("peer connection");

        assert!(Arc::ptr_eq(&pc_1, &pc_2), "re-offer must reuse the pc");
        assert_eq!(pc_2.get_transceivers().len(), 1);
        assert_ne!(
            sdp_attr(&answer_1, "ice-ufrag"),
            sdp_attr(&answer_2, "ice-ufrag"),
            "ICE restart must roll local credentials"
        );
        assert_ne!(
            sdp_attr(&answer_1, "ice-pwd"),
            sdp_attr(&answer_2, "ice-pwd")
        );
        assert_eq!(
            sdp_attr(&answer_1, "fingerprint"),
            sdp_attr(&answer_2, "fingerprint"),
            "DTLS identity must survive an ICE restart"
        );
    }

    use crate::media::Samples;
    use rustrtc::media::frame::MediaSample;

    /// Track with a captured outbound sample queue and the given remote SDP
    /// applied, so `send_packet` output can be inspected without a network.
    fn capture_track(sdp: &str) -> (RtcTrack, Arc<SampleStreamTrack>) {
        let mut track = RtcTrack::new(
            CancellationToken::new(),
            "capture".to_string(),
            TrackConfig::default(),
            RtcTrackConfig::default(),
        );
        track
            .parse_sdp_payload_types(rustrtc::SdpType::Offer, sdp)
            .expect("parse sdp");
        let (source, sink) = RtcTrack::create_audio_track(CodecType::PCMU, None);
        track.local_source = Some(source);
        (track, sink)
    }

    async fn drain(sink: &SampleStreamTrack) -> Vec<RtcAudioFrame> {
        let mut frames = Vec::new();
        while let Ok(Ok(sample)) =
            tokio::time::timeout(Duration::from_millis(20), sink.recv()).await
        {
            if let MediaSample::Audio(frame) = sample {
                frames.push(frame);
            }
        }
        frames
    }

    fn pcm_frame(sample_rate: u32) -> AudioFrame {
        AudioFrame {
            track_id: "tts".to_string(),
            samples: Samples::PCM {
                samples: vec![0; (sample_rate / 50) as usize],
            },
            sample_rate,
            channels: 1,
            ..Default::default()
        }
    }

    fn dtmf_frame(event: u8, duration_ms: u32, end: bool) -> AudioFrame {
        AudioFrame {
            track_id: "dtmf-track".to_string(),
            samples: Samples::Dtmf {
                event,
                duration_ms,
                end,
            },
            ..Default::default()
        }
    }

    /// (pt, ts, marker, event, end, duration)
    fn te_fields(frame: &RtcAudioFrame) -> (u8, u32, bool, u8, bool, u16) {
        let p = &frame.data;
        assert_eq!(p.len(), 4, "telephone-event payload");
        (
            frame.payload_type.unwrap(),
            frame.rtp_timestamp,
            frame.marker,
            p[0],
            p[1] & 0x80 != 0,
            u16::from_be_bytes([p[2], p[3]]),
        )
    }

    const PCMU_TE_SDP: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n";

    #[tokio::test]
    async fn test_dtmf_event_maps_to_rfc4733_packets() {
        let (mut track, sink) = capture_track(PCMU_TE_SDP);

        track.send_packet(&pcm_frame(8000)).await.unwrap();
        track.send_packet(&dtmf_frame(5, 20, false)).await.unwrap();
        // Audio during the event is muted.
        track.send_packet(&pcm_frame(8000)).await.unwrap();
        track.send_packet(&dtmf_frame(5, 40, false)).await.unwrap();
        track.send_packet(&dtmf_frame(5, 60, true)).await.unwrap();
        track.send_packet(&pcm_frame(8000)).await.unwrap();

        let frames = drain(&sink).await;
        assert_eq!(
            frames.len(),
            1 + 2 + 3 + 1,
            "audio, 2 updates, 3 ends, audio"
        );

        assert_eq!(frames[0].payload_type, Some(0));
        let event_ts = frames[0].rtp_timestamp.wrapping_add(160);

        let te: Vec<_> = frames[1..6].iter().map(te_fields).collect();
        assert_eq!(te[0], (101, event_ts, true, 5, false, 160));
        assert_eq!(te[1], (101, event_ts, false, 5, false, 320));
        for end in &te[2..] {
            assert_eq!(*end, (101, event_ts, false, 5, true, 480));
        }

        // Sequence numbers keep increasing across audio and events.
        let seqs: Vec<u16> = frames.iter().map(|f| f.sequence_number.unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
            "{seqs:?}"
        );

        // Audio resumes right after the event on the media timeline.
        let resumed = &frames[6];
        assert_eq!(resumed.payload_type, Some(0));
        assert!(!resumed.marker);
        assert_eq!(resumed.rtp_timestamp.wrapping_sub(event_ts), 480);
    }

    #[tokio::test]
    async fn test_dtmf_new_event_gets_new_timestamp() {
        let (mut track, sink) = capture_track(PCMU_TE_SDP);

        track.send_packet(&dtmf_frame(1, 20, true)).await.unwrap();
        track.send_packet(&dtmf_frame(1, 20, false)).await.unwrap();
        track.send_packet(&dtmf_frame(1, 40, true)).await.unwrap();

        let te: Vec<_> = drain(&sink).await.iter().map(te_fields).collect();
        assert_eq!(te.len(), 3 + 1 + 3);
        let (first_ts, second_ts) = (te[0].1, te[3].1);
        assert!(te[3].2, "second press starts with marker");
        assert_eq!(
            second_ts.wrapping_sub(first_ts),
            160,
            "{first_ts} -> {second_ts}"
        );
    }

    #[tokio::test]
    async fn test_dtmf_uses_matching_clock_and_skips_without_negotiation() {
        let opus_sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 111 101 110\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:101 telephone-event/8000\r\na=rtpmap:110 telephone-event/48000\r\n";
        let (mut track, sink) = capture_track(opus_sdp);
        track.send_packet(&dtmf_frame(3, 20, false)).await.unwrap();
        let te: Vec<_> = drain(&sink).await.iter().map(te_fields).collect();
        assert_eq!((te[0].0, te[0].5), (110, 960));

        let (mut track, sink) = capture_track(PCMU_SDP_1);
        track.send_packet(&dtmf_frame(3, 20, false)).await.unwrap();
        assert!(drain(&sink).await.is_empty());
        // No event was opened, so audio is not muted.
        track.send_packet(&pcm_frame(8000)).await.unwrap();
        assert_eq!(drain(&sink).await.len(), 1);
    }

    fn rtp_frame(payload_type: u8, payload: Vec<u8>) -> AudioFrame {
        AudioFrame {
            track_id: "peer".to_string(),
            samples: Samples::RTP {
                sequence_number: 9999,
                payload_type,
                payload,
            },
            ..Default::default()
        }
    }

    fn ts_deltas(frames: &[RtcAudioFrame]) -> Vec<u32> {
        frames
            .windows(2)
            .map(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp))
            .collect()
    }

    #[tokio::test]
    async fn test_timestamp_advances_by_audio_duration() {
        let (mut track, sink) = capture_track(PCMU_TE_SDP);

        // PCM at the internal 16kHz rate: 10ms and 20ms of audio.
        let pcm = |samples: usize| AudioFrame {
            samples: Samples::PCM {
                samples: vec![0; samples],
            },
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        };
        track.send_packet(&pcm(160)).await.unwrap();
        track.send_packet(&pcm(320)).await.unwrap();
        // 20ms at 8kHz.
        track.send_packet(&pcm_frame(8000)).await.unwrap();
        // Encoded RTP advances by the track ptime (20ms).
        track
            .send_packet(&rtp_frame(0, vec![0; 160]))
            .await
            .unwrap();
        track.send_packet(&pcm(320)).await.unwrap();
        track.send_packet(&pcm(320)).await.unwrap();

        let frames = drain(&sink).await;
        assert_eq!(frames.len(), 6);
        assert_eq!(ts_deltas(&frames), vec![80, 160, 160, 160, 160]);

        // Sequence numbers come from this track, not the source frame.
        let seqs: Vec<u16> = frames.iter().map(|f| f.sequence_number.unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
            "{seqs:?}"
        );
        assert!(!frames.iter().any(|f| f.marker));
    }

    #[tokio::test]
    async fn test_timestamp_reanchors_to_wall_clock_after_gap() {
        let (mut track, sink) = capture_track(PCMU_TE_SDP);

        track.send_packet(&pcm_frame(8000)).await.unwrap();
        let gap_start = Instant::now();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let gap = gap_start.elapsed();
        track.send_packet(&pcm_frame(8000)).await.unwrap();

        let frames = drain(&sink).await;
        assert_eq!(frames.len(), 2);
        assert!(frames[1].marker, "talkspurt after the gap is marked");
        // The second packet sits at the wall-clock position of the gap, not
        // gap + the first packet's 20ms.
        let delta = ts_deltas(&frames)[0];
        let expected = duration_to_rtp_ticks(gap, 8000);
        assert!(
            delta.abs_diff(expected) <= 40,
            "delta {delta}, expected ~{expected}"
        );
    }

    #[tokio::test]
    async fn test_timestamp_corrects_accumulated_drift() {
        let (mut track, _sink) = capture_track(PCMU_TE_SDP);
        let started = Instant::now();
        let (start, _) = track.next_media_timestamp(8000, Duration::from_millis(20));

        // A producer emitting 20ms packets every ~25ms falls further behind
        // the wall clock each packet, while no single packet is a gap.
        let mut jumped = None;
        for i in 1..=20 {
            std::thread::sleep(Duration::from_millis(25));
            let (ts, marker) = track.next_media_timestamp(8000, Duration::from_millis(20));
            if marker {
                jumped = Some((i, ts, started.elapsed()));
                break;
            }
        }
        let (i, ts, wall) = jumped.expect("drift beyond the threshold must be corrected");
        assert!(i > 1, "a single late packet is not drift");

        // The corrected packet sits at the wall-clock position.
        let clock = ts.wrapping_sub(start);
        let expected = duration_to_rtp_ticks(wall, 8000);
        assert!(
            clock.abs_diff(expected) <= 40,
            "clock {clock}, wall ~{expected}"
        );
    }

    #[tokio::test]
    async fn test_clock_rate_change_reanchors_without_jump() {
        let (mut track, _sink) = capture_track(PCMU_TE_SDP);
        track.next_media_timestamp(8000, Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Elapsed time measured in the new clock would look like a large
        // gap if the old anchor were reused.
        let (_, marker) = track.next_media_timestamp(48000, Duration::from_millis(20));
        assert!(!marker);
        assert_eq!(track.anchor.clock_rate, 48000);
    }

    async fn offer_with_codecs(codecs: Vec<CodecType>) -> String {
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = rustrtc::TransportMode::Rtp;
        rtc_config.codecs = codecs;
        let mut track = RtcTrack::new(
            CancellationToken::new(),
            "offer".to_string(),
            TrackConfig::default(),
            rtc_config,
        );
        track.create().await.expect("create peer connection");
        track.local_description().await.expect("local offer")
    }

    /// Payload types of the offer's audio m-line, in order.
    async fn offer_payload_types(codecs: Vec<CodecType>) -> Vec<String> {
        let offer = offer_with_codecs(codecs).await;
        offer
            .lines()
            .find_map(|l| l.strip_prefix("m=audio "))
            .expect("audio m-line")
            .split_whitespace()
            .skip(2)
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn test_offer_derives_telephone_event_per_clock_rate() {
        use CodecType::*;
        let cases: Vec<(Vec<CodecType>, Vec<&str>)> = vec![
            // 8kHz codecs get telephone-event/8000 (101), once.
            (vec![PCMU, PCMA], vec!["0", "8", "101"]),
            // Opus gets telephone-event/48000 (110).
            (vec![Opus], vec!["111", "110"]),
            // Both clocks, events in order of first appearance.
            (vec![PCMU, Opus, G722], vec!["0", "111", "9", "101", "110"]),
            (vec![Opus, PCMU], vec!["111", "0", "110", "101"]),
            // A configured telephone_event entry is ignored, not duplicated.
            (vec![TelephoneEvent, PCMU, TelephoneEvent], vec!["0", "101"]),
            // No configured codecs: rustrtc's default audio set plus events.
            (vec![], vec!["111", "0", "110", "101"]),
        ];
        for (codecs, expected) in cases {
            let pts = offer_payload_types(codecs.clone()).await;
            assert_eq!(pts, expected, "codecs {codecs:?}");
        }
        let offer = offer_with_codecs(vec![Opus, PCMU]).await;
        assert!(
            offer.contains("a=rtpmap:110 telephone-event/48000"),
            "{offer}"
        );
        assert!(
            offer.contains("a=rtpmap:101 telephone-event/8000"),
            "{offer}"
        );
    }

    #[test]
    fn test_inbound_telephone_event_on_dynamic_pt_is_not_decoded() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 111 110\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:110 telephone-event/48000\r\n";
        let (mut track, _sink) = capture_track(sdp);
        let mut frame = AudioFrame {
            samples: Samples::RTP {
                sequence_number: 1,
                payload_type: 110,
                payload: vec![5, 0x0a, 0x03, 0xc0],
            },
            sample_rate: 48000,
            channels: 1,
            ..Default::default()
        };
        track.processor_chain.process_frame(&mut frame).unwrap();
        assert!(
            matches!(
                frame.samples,
                Samples::RTP {
                    payload_type: 110,
                    ..
                }
            ),
            "telephone-event must stay RTP so DTMF detection sees it"
        );
    }

    /// Build an RTP-mode RtcTrack that has already generated its local offer,
    /// returning the track together with its peer connection.
    async fn rtp_track_with_local_offer(id: &str) -> RtcTrack {
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = rustrtc::TransportMode::Rtp;
        rtc_config.preferred_codec = Some(CodecType::PCMU);
        rtc_config.codecs = vec![CodecType::PCMU, CodecType::PCMA];

        let mut track = RtcTrack::new(
            CancellationToken::new(),
            id.to_string(),
            TrackConfig {
                codec: CodecType::PCMU,
                samplerate: 8000,
                ..Default::default()
            },
            rtc_config,
        );
        track.create().await.expect("create peer connection");
        track.local_description().await.expect("local offer");
        track
    }

    const PCMU_SDP_1: &str = "v=0\r\n\
        o=- 0 0 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 10000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n";

    const PCMU_SDP_2: &str = "v=0\r\n\
        o=- 0 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n";

    fn rtp_timeout_track(id: &str, rtp_timeout: Option<Duration>) -> RtcTrack {
        RtcTrack::new(
            CancellationToken::new(),
            id.to_string(),
            TrackConfig::default(),
            RtcTrackConfig {
                mode: TransportMode::Rtp,
                rtp_timeout,
                ..Default::default()
            },
        )
    }

    async fn advance_secs(secs: u64) {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(secs)).await;
        tokio::task::yield_now().await;
    }

    /// Outbound: the peer's final answer arms the timeout; early media does not.
    #[tokio::test(start_paused = true)]
    async fn rtp_timeout_arms_on_final_answer() {
        let mut track = rtp_timeout_track("timeout-track", Some(Duration::from_secs(2)));
        track.create().await.unwrap();
        track.local_description().await.unwrap();
        let (events, mut receiver) = tokio::sync::broadcast::channel(16);
        let (packets, _packet_receiver) = tokio::sync::mpsc::unbounded_channel();
        track.start(events, packets).await.unwrap();

        advance_secs(60).await;
        assert!(receiver.try_recv().is_err(), "unarmed before answer");

        track
            .update_remote_description_provisional(&PCMU_SDP_1.to_string())
            .await
            .unwrap();
        advance_secs(5).await;
        assert!(receiver.try_recv().is_err(), "early media must not arm");

        // Same SDP as the 183: still arms despite the SDP-unchanged fast path.
        track
            .update_remote_description(&PCMU_SDP_1.to_string())
            .await
            .unwrap();
        advance_secs(1).await;
        assert!(receiver.try_recv().is_err());
        advance_secs(2).await;
        let event = receiver.try_recv().expect("timeout without a first sample");
        assert!(matches!(
            &event,
            SessionEvent::RtpTimeout { track_id, timeout: 2, .. } if track_id == "timeout-track"
        ));
        assert_eq!(serde_json::to_value(event).unwrap()["event"], "rtpTimeout");
        assert!(
            !track.cancel_token.is_cancelled(),
            "must not close the call"
        );
        advance_secs(5).await;
        assert!(receiver.try_recv().is_err(), "once per gap");

        // Any sample, even silence, rearms.
        let (source, sample_track, _) = sample_track(rustrtc::media::MediaKind::Audio, 8);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(RtcTrack::run_receiving_worker(
            sample_track,
            tx,
            track.track_id.clone(),
            track.rtp_timeout.clone(),
        ));
        source
            .send_audio(RtcAudioFrame {
                data: Bytes::from(vec![0xff; 160]),
                payload_type: Some(0),
                clock_rate: 8000,
                ..Default::default()
            })
            .unwrap();
        rx.recv().await.unwrap();
        advance_secs(1).await;
        assert!(receiver.try_recv().is_err());
        advance_secs(2).await;
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SessionEvent::RtpTimeout { .. }
        ));

        // Peer recvonly/inactive suspends; resuming grants a fresh timeout.
        for direction in ["recvonly", "inactive"] {
            let hold = format!("{}a={}\r\n", PCMU_SDP_1, direction);
            track.update_remote_description_force(&hold).await.unwrap();
            advance_secs(10).await;
            assert!(receiver.try_recv().is_err(), "suspended while {direction}");
        }
        track
            .update_remote_description_force(&PCMU_SDP_1.to_string())
            .await
            .unwrap();
        advance_secs(1).await;
        assert!(receiver.try_recv().is_err());
        advance_secs(2).await;
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SessionEvent::RtpTimeout { .. }
        ));

        worker.abort();
        track.stop().await.unwrap();
        advance_secs(10).await;
        assert!(receiver.try_recv().is_err());
    }

    /// Inbound: answering the offer is not acceptance; `on_answered` is, and
    /// its timeout replaces the configured one.
    #[tokio::test(start_paused = true)]
    async fn rtp_timeout_arms_on_local_answer() {
        let secs = |s| Some(Duration::from_secs(s));
        // (configured, on_answered timeout, expected event timeout)
        for (configured, accepted, expected) in [
            (None, None, None),
            (secs(2), None, Some(2)),
            (None, secs(2), Some(2)),
            (secs(5), secs(2), Some(2)),
            (secs(2), secs(0), None),
        ] {
            let mut track = rtp_timeout_track("inbound", configured);
            track.handshake(PCMU_SDP_1.to_string(), None).await.unwrap();
            let (events, mut receiver) = tokio::sync::broadcast::channel(16);
            let (packets, _packet_receiver) = tokio::sync::mpsc::unbounded_channel();
            track.start(events, packets).await.unwrap();
            advance_secs(10).await;
            assert!(receiver.try_recv().is_err(), "handshake must not arm");

            track.on_answered(accepted);
            advance_secs(1).await;
            assert!(receiver.try_recv().is_err());
            advance_secs(2).await;
            match expected {
                Some(timeout) => assert!(matches!(
                    receiver.try_recv().unwrap(),
                    SessionEvent::RtpTimeout { timeout: t, .. } if t == timeout
                )),
                None => assert!(receiver.try_recv().is_err(), "None and zero disable it"),
            }
            track.stop().await.unwrap();
            tokio::task::yield_now().await;
        }
    }

    /// SIP 183 early media must be applied as a provisional answer (Pranswer),
    /// keeping the signaling state in HaveLocalOffer, so the final 200 OK can
    /// still complete the negotiation as a full Answer.
    #[tokio::test]
    async fn test_pranswer_keeps_local_offer_until_final_answer() {
        use rustrtc::SignalingState;

        let mut track = rtp_track_with_local_offer("test-pranswer").await;
        let pc = track.peer_connection.clone().expect("peer connection");

        assert_eq!(
            pc.signaling_state(),
            SignalingState::HaveLocalOffer,
            "after local offer"
        );

        track
            .update_remote_description_provisional(&PCMU_SDP_1.to_string())
            .await
            .expect("apply 183 as provisional answer");

        assert_eq!(
            pc.signaling_state(),
            SignalingState::HaveLocalOffer,
            "183 (Pranswer) must not finalize negotiation"
        );

        track
            .update_remote_description_force(&PCMU_SDP_2.to_string())
            .await
            .expect("apply 200 OK as final answer");

        assert_eq!(
            pc.signaling_state(),
            SignalingState::Stable,
            "final 200 OK answer must stabilize signaling"
        );
    }

    /// When the 200 OK carries no body after early media, the early SDP is
    /// re-applied as a final Answer. Even though it is byte-for-byte the same
    /// SDP as the provisional answer, the forced update must still transition
    /// the signaling state from HaveLocalOffer to Stable.
    #[tokio::test]
    async fn test_pranswer_finalized_with_force_even_when_sdp_unchanged() {
        use rustrtc::SignalingState;

        let mut track = rtp_track_with_local_offer("test-pranswer-force").await;
        let pc = track.peer_connection.clone().expect("peer connection");

        track
            .update_remote_description_provisional(&PCMU_SDP_1.to_string())
            .await
            .expect("apply 183 as provisional answer");
        assert_eq!(pc.signaling_state(), SignalingState::HaveLocalOffer);

        // Final answer resolves to the same SDP (empty 200 OK body fallback).
        track
            .update_remote_description_force(&PCMU_SDP_1.to_string())
            .await
            .expect("re-apply early SDP as final answer");

        assert_eq!(
            pc.signaling_state(),
            SignalingState::Stable,
            "forced final answer must stabilize even with unchanged SDP"
        );
    }
}
