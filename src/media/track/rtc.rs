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
    AudioCapability, IceServer, PeerConnection, PeerConnectionEvent, PeerConnectionState,
    RtcConfiguration, RtpCodecParameters, TransportMode,
    config::MediaCapabilities,
    media::{
        MediaStreamTrack, SampleStreamSource,
        frame::{AudioFrame as RtcAudioFrame, MediaKind},
        sample_track,
        track::SampleStreamTrack,
    },
};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Clone)]
pub struct RtcTrackConfig {
    pub mode: TransportMode,
    pub ice_servers: Option<Vec<IceServer>>,
    pub local_addr: Option<IpAddr>,
    pub rtp_port_range: Option<(u16, u16)>,
    pub preferred_codec: Option<CodecType>,
    pub codecs: Vec<CodecType>,
    pub payload_type: Option<u8>,
}

impl Default for RtcTrackConfig {
    fn default() -> Self {
        Self {
            mode: TransportMode::WebRtc, // Default WebRTC behavior
            ice_servers: None,
            local_addr: None,
            rtp_port_range: None,
            preferred_codec: None,
            codecs: Vec::new(),
            payload_type: None,
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
    payload_type: Arc<std::sync::atomic::AtomicU8>,
    pub peer_connection: Option<Arc<PeerConnection>>,
    next_rtp_timestamp: Arc<std::sync::atomic::AtomicU32>,
    next_rtp_sequence_number: Arc<std::sync::atomic::AtomicU16>,
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
            payload_type: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            peer_connection: None,
            next_rtp_timestamp: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            next_rtp_sequence_number: Arc::new(std::sync::atomic::AtomicU16::new(0)),
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
        let (source, track, _) = sample_track(MediaKind::Audio, 100);
        (Arc::new(source), track)
    }

    pub fn local_description(&self) -> Result<String> {
        let pc = self
            .peer_connection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No PeerConnection"))?;
        let offer = pc.create_offer()?;
        pc.set_local_description(offer.clone())?;
        Ok(offer.to_sdp_string())
    }

    pub async fn create(&mut self) -> Result<()> {
        if self.peer_connection.is_some() {
            return Ok(());
        }

        let mut config = RtcConfiguration::default();
        config.transport_mode = self.rtc_config.mode.clone();

        if let Some(ice_servers) = &self.rtc_config.ice_servers {
            config.ice_servers = ice_servers.clone();
        }

        if !self.rtc_config.codecs.is_empty() {
            let mut caps = MediaCapabilities::default();
            caps.audio.clear();
            for codec in &self.rtc_config.codecs {
                let cap = match codec {
                    CodecType::PCMU => AudioCapability::pcmu(),
                    CodecType::PCMA => AudioCapability::pcma(),
                    CodecType::G722 => AudioCapability::g722(),
                    CodecType::G729 => AudioCapability {
                        payload_type: codec.payload_type(),
                        codec_name: "G729".to_string(),
                        clock_rate: codec.clock_rate(),
                        channels: codec.channels() as u8,
                        fmtp: codec.fmtp().map(Into::into),
                        rtcp_fbs: vec![],
                    },
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

        self.payload_type
            .store(payload_type, std::sync::atomic::Ordering::SeqCst);

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

        if self.rtc_config.mode == TransportMode::Rtp {
            for transceiver in peer_connection.get_transceivers() {
                if let Some(receiver) = transceiver.receiver() {
                    let track = receiver.track();
                    info!(track_id=%self.track_id, "RTP mode: starting receiver track handler");
                    Self::spawn_track_handler(
                        track,
                        self.packet_sender.clone(),
                        self.track_id.clone(),
                        self.cancel_token.clone(),
                        self.processor_chain.clone(),
                        payload_type,
                    );
                }
            }
        }

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
        tokio::spawn(async move {
            info!(track_id=%track_id_log, "RtcTrack event loop started");
            let mut events = futures::stream::unfold(pc_clone, |pc| async move {
                pc.recv().await.map(|ev| (ev, pc))
            })
            .take_until(cancel_token.cancelled())
            .boxed();

            while let Some(event) = events.next().await {
                if let PeerConnectionEvent::Track(transceiver) = event {
                    if let Some(receiver) = transceiver.receiver() {
                        let track = receiver.track();
                        info!(track_id=%track_id_log, "New track received");

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
            debug!(track_id=%track_id_log, "RtcTrack event loop ended");
        });

        // 2. State Monitoring
        if is_webrtc {
            let pc_state = pc.clone();
            let cancel_token_state = self.cancel_token.clone();
            let mut state_rx = pc_state.subscribe_peer_state();
            let track_id_state = track_id.clone();

            tokio::spawn(async move {
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
        let processor_chain_proc = processor_chain.clone();
        let cancel_token_proc = cancel_token.clone();
        tokio::spawn(async move {
            info!(track_id=%track_id_proc, "RtcTrack processing worker started");
            while let Some(frame) = rx.recv().await {
                if cancel_token_proc.is_cancelled() {
                    break;
                }
                Self::process_audio_frame(
                    frame,
                    &track_id_proc,
                    &packet_sender_proc,
                    &processor_chain_proc,
                    default_payload_type,
                )
                .await;
            }
            info!(track_id=%track_id_proc, "RtcTrack processing worker stopped");
        });

        // Receiving Worker
        tokio::spawn(async move {
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
                }
            }
        });
    }

    async fn process_audio_frame(
        frame: rustrtc::media::frame::AudioFrame,
        track_id: &TrackId,
        packet_sender: &Arc<Mutex<Option<TrackPacketSender>>>,
        processor_chain: &ProcessorChain,
        default_payload_type: u8,
    ) {
        let packet_sender = packet_sender.lock().await;
        if let Some(sender) = packet_sender.as_ref() {
            let mut af = AudioFrame {
                track_id: track_id.clone(),
                samples: crate::media::Samples::RTP {
                    payload_type: frame.payload_type.unwrap_or(default_payload_type),
                    payload: frame.data.to_vec(),
                    sequence_number: frame.sequence_number.unwrap_or(0),
                },
                timestamp: crate::media::get_timestamp(),
                sample_rate: frame.sample_rate,
                channels: frame.channels as u16,
            };

            if let Err(e) = processor_chain.process_frame(&mut af) {
                debug!(track_id=%track_id, "processor_chain process_frame error: {:?}", e);
            }

            sender.send(af).ok();
        }
    }

    pub fn parse_sdp_payload_types(&self, sdp: &str) {
        let patterns = [
            ("opus", CodecType::Opus),
            ("pcmu", CodecType::PCMU),
            ("pcma", CodecType::PCMA),
            ("g722", CodecType::G722),
            ("g729", CodecType::G729),
            ("telephone-event", CodecType::TelephoneEvent),
        ];

        for (name, codec_type) in patterns {
            let re = format!(r"(?i)a=rtpmap:(\d+)\s+{}/", name);
            if let Ok(reg) = regex::Regex::new(&re) {
                if let Some(cap) = reg.captures(sdp) {
                    if let Ok(pt) = cap[1].parse::<u8>() {
                        info!(track_id=%self.track_id, "Negotiated payload type for {}: {}", name, pt);
                        self.payload_type
                            .store(pt, std::sync::atomic::Ordering::SeqCst);
                        self.encoder.set_payload_type(pt, codec_type.clone());
                        self.processor_chain
                            .codec
                            .lock()
                            .unwrap()
                            .set_payload_type(pt, codec_type);
                    }
                }
            }
        }
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

        let pc = self.peer_connection.as_ref().unwrap();

        let sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer)?;
        pc.set_remote_description(sdp).await?;

        // Extract negotiated payload types from OFFER
        self.parse_sdp_payload_types(&offer);

        let answer = pc.create_answer()?;
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
        if let Some(pc) = &self.peer_connection {
            let sdp_obj = rustrtc::SessionDescription::parse(rustrtc::SdpType::Answer, answer)?;
            pc.set_remote_description(sdp_obj).await?;

            // Extract negotiated payload types from SDP string
            self.parse_sdp_payload_types(answer);
        }
        Ok(())
    }

    async fn start(
        &self,
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
            tokio::spawn(async move {
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

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        let packet = packet.clone();

        if let Some(source) = &self.local_source {
            match &packet.samples {
                crate::media::Samples::PCM { samples } => {
                    let payload_type = self.get_payload_type();
                    let (_, encoded) = self.encoder.encode(payload_type, packet.clone());

                    if !encoded.is_empty() {
                        let target_sample_rate = match payload_type {
                            0 | 8 | 18 => 8000,
                            9 => 16000,
                            111 => 48000,
                            _ => packet.sample_rate,
                        };
                        let target_samples = (samples.len() as u64 * target_sample_rate as u64
                            / packet.sample_rate as u64)
                            as u32;
                        let sample_count_per_channel =
                            target_samples / self.track_config.channels as u32;
                        let rtp_timestamp = self.next_rtp_timestamp.fetch_add(
                            sample_count_per_channel,
                            std::sync::atomic::Ordering::SeqCst,
                        );
                        let sequence_number = self
                            .next_rtp_sequence_number
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                        let frame = RtcAudioFrame {
                            data: Bytes::from(encoded),
                            sample_rate: target_sample_rate,
                            channels: self.track_config.channels as u8,
                            samples: sample_count_per_channel,
                            payload_type: Some(payload_type),
                            sequence_number: Some(sequence_number),
                            rtp_timestamp,
                        };
                        source.send_audio(frame).await.ok();
                    }
                }
                crate::media::Samples::RTP {
                    payload,
                    payload_type,
                    sequence_number,
                } => {
                    let target_sample_rate = match *payload_type {
                        0 | 8 | 18 => 8000,
                        9 => 16000,
                        111 => 48000,
                        _ => packet.sample_rate,
                    };

                    // Estimate samples if we don't have them
                    let increment = match *payload_type {
                        0 | 8 | 18 => payload.len() as u32,
                        9 => (payload.len() * 2) as u32,
                        111 => (target_sample_rate / 50) as u32, // Assume 20ms for Opus if unknown
                        _ => (target_sample_rate / 50) as u32,
                    };
                    let rtp_timestamp = self
                        .next_rtp_timestamp
                        .fetch_add(increment, std::sync::atomic::Ordering::SeqCst);
                    let sequence_number = *sequence_number;

                    let frame = RtcAudioFrame {
                        data: Bytes::from(payload.clone()),
                        sample_rate: target_sample_rate,
                        channels: self.track_config.channels as u8,
                        samples: 0, // Not used for RTP
                        payload_type: Some(*payload_type),
                        sequence_number: Some(sequence_number),
                        rtp_timestamp,
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
        let pt = self.payload_type.load(std::sync::atomic::Ordering::SeqCst);
        if pt != 0 {
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
