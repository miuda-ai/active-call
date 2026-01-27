use super::track_codec::TrackCodec;
use crate::{
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::{
        processor::ProcessorChain,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use audio_codec::CodecType;
use bytes::Bytes;
use futures::StreamExt;
use rustrtc::{
    AudioCapability, IceServer, MediaKind, PeerConnection, PeerConnectionEvent,
    PeerConnectionState, RtcConfiguration, RtpCodecParameters, SdpType, TransportMode,
    config::MediaCapabilities,
    media::{
        MediaStreamTrack, SampleStreamSource, frame::AudioFrame as RtcAudioFrame, sample_track,
        track::SampleStreamTrack,
    },
};
use std::{
    sync::Arc,
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
    pub preferred_codec: Option<CodecType>,
    pub codecs: Vec<CodecType>,
    pub payload_type: Option<u8>,
    pub enable_latching: Option<bool>,
}

impl Default for RtcTrackConfig {
    fn default() -> Self {
        Self {
            mode: TransportMode::WebRtc, // Default WebRTC behavior
            ice_servers: None,
            external_ip: None,
            rtp_port_range: None,
            preferred_codec: None,
            codecs: Vec::new(),
            payload_type: None,
            enable_latching: None,
        }
    }
}

pub struct RtcTrack {
    track_id: TrackId,
    track_config: TrackConfig,
    rtc_config: RtcTrackConfig,
    processor_chain: ProcessorChain,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    cancel_token: CancellationToken,
    local_source: Option<Arc<SampleStreamSource>>,
    encoder: TrackCodec,
    ssrc: u32,
    payload_type: Option<u8>,
    pub peer_connection: Option<Arc<PeerConnection>>,
    next_rtp_timestamp: u32,
    next_rtp_sequence_number: u16,
    last_packet_time: Option<Instant>,
    last_remote_sdp: Option<String>,
    need_marker: bool,
}

impl RtcTrack {
    pub fn new(
        cancel_token: CancellationToken,
        id: TrackId,
        track_config: TrackConfig,
        rtc_config: RtcTrackConfig,
    ) -> Self {
        let processor_chain = ProcessorChain::new(track_config.samplerate);
        Self {
            track_id: id,
            track_config,
            rtc_config,
            processor_chain,
            packet_sender: Arc::new(Mutex::new(None)),
            cancel_token,
            local_source: None,
            encoder: TrackCodec::new(),
            ssrc: 0,
            payload_type: None,
            peer_connection: None,
            next_rtp_timestamp: 0,
            next_rtp_sequence_number: 0,
            last_packet_time: None,
            last_remote_sdp: None,
            need_marker: false,
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

        config.enable_latching = self
            .rtc_config
            .enable_latching
            .unwrap_or_else(|| self.rtc_config.mode == TransportMode::Rtp);

        if !self.rtc_config.codecs.is_empty() {
            let mut caps = MediaCapabilities::default();
            caps.audio.clear();

            for codec in &self.rtc_config.codecs {
                let cap = match codec {
                    CodecType::PCMU => AudioCapability::pcmu(),
                    CodecType::PCMA => AudioCapability::pcma(),
                    CodecType::G722 => AudioCapability::g722(),
                    CodecType::G729 => AudioCapability::g729(),
                    CodecType::TelephoneEvent => AudioCapability::telephone_event(),
                    #[cfg(feature = "opus")]
                    CodecType::Opus => AudioCapability::opus(),
                };
                caps.audio.push(cap);
            }
            config.media_capabilities = Some(caps);
        }

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
        );

        Ok(())
    }

    fn spawn_handlers(
        &self,
        pc: Arc<PeerConnection>,
        track_id: TrackId,
        processor_chain: ProcessorChain,
        default_payload_type: u8,
    ) {
        let cancel_token = self.cancel_token.clone();
        let packet_sender = self.packet_sender.clone();
        let pc_clone = pc.clone();
        let track_id_log = track_id.clone();
        let is_webrtc = self.rtc_config.mode != TransportMode::Rtp;

        // 1. Event Loop
        crate::spawn(async move {
            info!(track_id=%track_id_log, "RtcTrack event loop started");
            let mut events = futures::stream::unfold(pc_clone.clone(), |pc| async move {
                pc.recv().await.map(|ev| (ev, pc))
            })
            .take_until(cancel_token.cancelled())
            .boxed();

            let mut event_count = 0;
            while let Some(event) = events.next().await {
                event_count += 1;
                let event_type = match &event {
                    PeerConnectionEvent::Track(_) => "Track",
                    PeerConnectionEvent::DataChannel(_) => "DataChannel",
                };
                debug!(track_id=%track_id_log, "Received PeerConnectionEvent #{}: {}", event_count, event_type);

                if let PeerConnectionEvent::Track(transceiver) = event {
                    if let Some(receiver) = transceiver.receiver() {
                        let track = receiver.track();
                        info!(track_id=%track_id_log, "New track received (SSRC latching complete)");

                        Self::spawn_track_handler(
                            track,
                            packet_sender.clone(),
                            track_id_log.clone(),
                            cancel_token.clone(),
                            processor_chain.clone(),
                            default_payload_type,
                        );
                    }
                }
            }
            debug!(track_id=%track_id_log, "RtcTrack event loop ended, total events: {}", event_count);
        });

        // 2. State Monitoring
        if is_webrtc {
            let pc_state = pc.clone();
            let cancel_token_state = self.cancel_token.clone();
            let mut state_rx = pc_state.subscribe_peer_state();
            let track_id_state = track_id.clone();

            crate::spawn(async move {
                while state_rx.changed().await.is_ok() {
                    let s = *state_rx.borrow();
                    debug!(track_id=%track_id_state, "peer connection state changed: {:?}", s);
                    match s {
                        PeerConnectionState::Disconnected
                        | PeerConnectionState::Closed
                        | PeerConnectionState::Failed => {
                            info!(
                                track_id = %track_id_state,
                                "peer connection is {:?}, try to close", s
                            );
                            cancel_token_state.cancel();
                            pc_state.close();
                            break;
                        }
                        _ => {}
                    }
                }
            });
        }

        // 3. Stats Loop
        let pc_stats = pc.clone();
        let track_id_stats = track_id.clone();
        let cancel_token_stats = self.cancel_token.clone();
        crate::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = cancel_token_stats.cancelled() => break,
                    _ = interval.tick() => {
                        match pc_stats.get_stats().await {
                            Ok(stats) => {
                                info!(track_id=%track_id_stats, "RTCP Stats: {:?}", stats);
                            }
                            Err(e) => {
                                debug!(track_id=%track_id_stats, "Failed to get stats: {:?}", e);
                            }
                        }
                    }
                }
            }
        });
    }

    fn spawn_track_handler(
        track: Arc<SampleStreamTrack>,
        packet_sender_arc: Arc<Mutex<Option<TrackPacketSender>>>,
        track_id: TrackId,
        cancel_token: CancellationToken,
        processor_chain: ProcessorChain,
        default_payload_type: u8,
    ) {
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<rustrtc::media::frame::AudioFrame>();

        // Processing Worker
        let track_id_proc = track_id.clone();
        let packet_sender_proc = packet_sender_arc.clone();
        let mut processor_chain_proc = processor_chain.clone();
        let cancel_token_proc = cancel_token.clone();
        crate::spawn(async move {
            info!(track_id=%track_id_proc, "RtcTrack processing worker started");
            while let Some(frame) = rx.recv().await {
                if cancel_token_proc.is_cancelled() {
                    break;
                }
                Self::process_audio_frame(
                    frame,
                    &track_id_proc,
                    &packet_sender_proc,
                    &mut processor_chain_proc,
                    default_payload_type,
                )
                .await;
            }
            info!(track_id=%track_id_proc, "RtcTrack processing worker stopped");
        });

        // Receiving Worker
        let track_id_recv = track_id.clone();
        crate::spawn(async move {
            let mut samples =
                futures::stream::unfold(
                    track,
                    |t| async move { t.recv().await.ok().map(|s| (s, t)) },
                )
                .take_until(cancel_token.cancelled())
                .boxed();

            while let Some(sample) = samples.next().await {
                if let rustrtc::media::frame::MediaSample::Audio(frame) = sample {
                    if let Err(_) = tx.send(frame) {
                        break;
                    }
                } else {
                    debug!(track_id=%track_id_recv, "Received non-audio sample");
                }
            }
            info!(track_id=%track_id_recv, "RtcTrack receiving worker stopped");
        });
    }

    async fn process_audio_frame(
        frame: rustrtc::media::frame::AudioFrame,
        track_id: &TrackId,
        packet_sender: &Arc<Mutex<Option<TrackPacketSender>>>,
        processor_chain: &mut ProcessorChain,
        default_payload_type: u8,
    ) {
        let packet_sender = packet_sender.lock().await;
        if let Some(sender) = packet_sender.as_ref() {
            let payload_type = frame.payload_type.unwrap_or(default_payload_type);
            let src_codec = match CodecType::try_from(payload_type) {
                Ok(c) => c,
                Err(_) => {
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
            };
            if let Err(e) = processor_chain.process_frame(&mut af) {
                debug!(track_id=%track_id, "processor_chain process_frame error: {:?}", e);
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
            for attr in &media.attributes {
                if attr.key == "rtpmap" {
                    if let Some(value) = &attr.value {
                        if let Ok((pt, codec, _, _)) = parse_rtpmap(value) {
                            self.encoder.set_payload_type(pt, codec.clone());
                            self.processor_chain.codec.set_payload_type(pt, codec);
                        }
                    }
                }
            }

            // Negotiate primary audio codec
            let mut negotiated = None;

            // If we are the offerer (receiving an Answer), we prioritize our own preferred codec order
            // that is also present in the answer.
            if sdp_type == rustrtc::sdp::SdpType::Answer && !self.rtc_config.codecs.is_empty() {
                for preferred_codec in &self.rtc_config.codecs {
                    if *preferred_codec == CodecType::TelephoneEvent {
                        continue;
                    }
                    for fmt in &media.formats {
                        if let Ok(pt) = fmt.parse::<u8>() {
                            let codec = self
                                .encoder
                                .payload_type_map
                                .get(&pt)
                                .cloned()
                                .or_else(|| CodecType::try_from(pt).ok());
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
                        let codec = self
                            .encoder
                            .payload_type_map
                            .get(&pt)
                            .cloned()
                            .or_else(|| CodecType::try_from(pt).ok());

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

    async fn update_remote_description_internal(
        &mut self,
        answer: &String,
        force_update: bool,
    ) -> Result<()> {
        info!(
            track_id=%self.track_id,
            "update_remote_description_internal called. force={}, last_sdp_is_some={}, mode={:?}",
            force_update,
            self.last_remote_sdp.is_some(),
            self.rtc_config.mode
        );

        if let Some(pc) = &self.peer_connection {
            if !force_update {
                if let Some(ref last_sdp) = self.last_remote_sdp {
                    if Self::normalize_sdp(last_sdp) == Self::normalize_sdp(answer) {
                        debug!(track_id=%self.track_id, "SDP unchanged, skipping update_remote_description");
                        return Ok(());
                    }
                }
            } else {
                debug!(track_id=%self.track_id, "Force update requested, skipping SDP comparison");
            }

            let _is_first_remote_sdp = self.last_remote_sdp.is_none();

            let sdp_obj = rustrtc::SessionDescription::parse(rustrtc::SdpType::Answer, answer)?;
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
                        pc.set_remote_description(sdp_obj).await?;
                        self.last_remote_sdp = Some(answer.clone());
                        info!(track_id=%self.track_id, "successfully re-synced WebRTC state for SIP update");
                    } else {
                        return Err(e.into());
                    }
                }
            }

            // Track events will be handled by the event loop after SSRC latching

            // Extract negotiated payload types from SDP string
            self.parse_sdp_payload_types(rustrtc::SdpType::Answer, answer)?;
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

        pc.set_local_description(answer.clone())?;

        if self.rtc_config.mode != TransportMode::Rtp {
            pc.wait_for_gathering_complete().await;
        }

        let final_answer = pc
            .local_description()
            .ok_or(anyhow::anyhow!("No local description"))?;

        Ok(final_answer.to_sdp_string())
    }

    async fn update_remote_description(&mut self, answer: &String) -> Result<()> {
        self.update_remote_description_internal(answer, false).await
    }

    async fn update_remote_description_force(&mut self, answer: &String) -> Result<()> {
        self.update_remote_description_internal(answer, true).await
    }

    async fn start(
        &mut self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        *self.packet_sender.lock().await = Some(packet_sender.clone());
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
        let packet = packet.clone();

        if let Some(source) = &self.local_source {
            match &packet.samples {
                crate::media::Samples::PCM { samples } => {
                    let payload_type = self.get_payload_type();
                    let (_, encoded) = self.encoder.encode(payload_type, packet.clone());
                    let target_codec = CodecType::try_from(payload_type)?;
                    if !encoded.is_empty() {
                        let clock_rate = target_codec.clock_rate();

                        let now = Instant::now();
                        if let Some(last_time) = self.last_packet_time {
                            let elapsed = now.duration_since(last_time);
                            if elapsed.as_millis() > 50 {
                                let gap_increment =
                                    (elapsed.as_millis() as u32 * clock_rate) / 1000;
                                self.next_rtp_timestamp += gap_increment;
                                self.need_marker = true;
                            }
                        }

                        self.last_packet_time = Some(now);

                        let timestamp_increment = (samples.len() as u64 * clock_rate as u64
                            / packet.sample_rate as u64
                            / self.track_config.channels as u64)
                            as u32;
                        let rtp_timestamp = self.next_rtp_timestamp;
                        self.next_rtp_timestamp += timestamp_increment;
                        let sequence_number = self.next_rtp_sequence_number;
                        self.next_rtp_sequence_number += 1;

                        let mut marker = false;
                        if self.need_marker {
                            marker = true;
                            self.need_marker = false;
                        }

                        let frame = RtcAudioFrame {
                            data: Bytes::from(encoded),
                            clock_rate,
                            payload_type: Some(payload_type),
                            sequence_number: Some(sequence_number),
                            rtp_timestamp,
                            marker,
                            ..Default::default()
                        };
                        source.send_audio(frame).await.ok();
                    }
                }
                crate::media::Samples::RTP {
                    payload,
                    payload_type,
                    sequence_number,
                } => {
                    let clock_rate = match *payload_type {
                        0 | 8 | 9 | 18 => 8000,
                        111 => 48000,
                        _ => packet.sample_rate,
                    };

                    let now = Instant::now();
                    if let Some(last_time) = self.last_packet_time {
                        let elapsed = now.duration_since(last_time);
                        if elapsed.as_millis() > 50 {
                            let gap_increment = (elapsed.as_millis() as u32 * clock_rate) / 1000;
                            self.next_rtp_timestamp += gap_increment;
                            self.need_marker = true;
                        }
                    }
                    self.last_packet_time = Some(now);

                    let increment = match *payload_type {
                        0 | 8 | 18 => payload.len() as u32,
                        9 => payload.len() as u32,
                        111 => (clock_rate / 50) as u32,
                        _ => (clock_rate / 50) as u32,
                    };

                    let rtp_timestamp = self.next_rtp_timestamp;
                    self.next_rtp_timestamp += increment;
                    let sequence_number = *sequence_number;

                    let mut marker = false;
                    if self.need_marker {
                        marker = true;
                        self.need_marker = false;
                    }

                    let frame = RtcAudioFrame {
                        data: Bytes::from(payload.clone()),
                        clock_rate,
                        payload_type: Some(*payload_type),
                        sequence_number: Some(sequence_number),
                        rtp_timestamp,
                        marker,
                        ..Default::default()
                    };
                    source.send_audio(frame).await.ok();
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl RtcTrack {
    fn get_payload_type(&self) -> u8 {
        if let Some(pt) = self.payload_type {
            return pt;
        }

        self.rtc_config.payload_type.unwrap_or_else(|| {
            match self.rtc_config.preferred_codec.unwrap_or(CodecType::Opus) {
                CodecType::PCMU => 0,
                CodecType::PCMA => 8,
                CodecType::Opus => 111,
                CodecType::G722 => 9,
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
    }

    #[tokio::test]
    async fn test_rtp_mode_handshake_spawns_handler() {
        use rustrtc::TransportMode;

        let track_id = "test-track-sip".to_string();
        let cancel = CancellationToken::new();
        let track_config = TrackConfig::default();
        let mut rtc_config = RtcTrackConfig::default();
        rtc_config.mode = TransportMode::Rtp;

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
        assert!(res.is_ok());

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
}
