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
use std::{sync::Arc, time::Duration};
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
        config.transport_mode = self.rtc_config.mode.clone();

        if let Some(ice_servers) = &self.rtc_config.ice_servers {
            config.ice_servers = ice_servers.clone();
        }

        if let Some(external_ip) = &self.rtc_config.external_ip {
            config.external_ip = Some(external_ip.clone());
        }

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
            self.payload_type.clone(),
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
                        self.payload_type.clone(),
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
        default_payload_type: Arc<std::sync::atomic::AtomicU8>,
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
                            default_payload_type.clone(),
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
        default_payload_type: Arc<std::sync::atomic::AtomicU8>,
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
                    default_payload_type.clone(),
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
        default_payload_type: Arc<std::sync::atomic::AtomicU8>,
    ) {
        let packet_sender = packet_sender.lock().await;
        if let Some(sender) = packet_sender.as_ref() {
            let payload_type = frame
                .payload_type
                .unwrap_or_else(|| default_payload_type.load(std::sync::atomic::Ordering::SeqCst));
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

            for fmt in &media.formats {
                if let Ok(pt) = fmt.parse::<u8>() {
                    let codec = self
                        .encoder
                        .payload_type_map
                        .borrow()
                        .get(&pt)
                        .cloned()
                        .or_else(|| CodecType::try_from(pt).ok());

                    if let Some(codec) = codec {
                        if codec != CodecType::TelephoneEvent {
                            info!(track_id=%self.track_id, "Negotiated primary audio PT {} ({:?})", pt, codec);
                            self.payload_type
                                .store(pt, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
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

        let sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer)?;
        pc.set_remote_description(sdp).await?;

        self.parse_sdp_payload_types(rustrtc::SdpType::Offer, &offer)?;

        let answer = pc.create_answer().await?;
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
            self.parse_sdp_payload_types(rustrtc::SdpType::Answer, answer)?;
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
                    let target_codec = CodecType::try_from(payload_type)?;
                    if !encoded.is_empty() {
                        let target_samples =
                            (samples.len() as u64 * target_codec.samplerate() as u64
                                / packet.sample_rate as u64) as u32;
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
                            clock_rate: target_codec.clock_rate(),
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
                        clock_rate: target_sample_rate,
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
}
