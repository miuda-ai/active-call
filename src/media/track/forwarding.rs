use crate::event::EventSender;
use crate::media::processor::ProcessorChain;
use crate::media::track::{Track, TrackConfig, TrackPacketSender};
use crate::media::{AudioFrame, Samples, TrackId};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct ForwardingTrack {
    track_id: TrackId,
    source_peer_track_id: TrackId,
    peer_sender: mpsc::Sender<AudioFrame>,
    inbound_receiver: Option<mpsc::Receiver<AudioFrame>>,
    processor_chain: ProcessorChain,
    config: TrackConfig,
    cancel_token: CancellationToken,
    ssrc: u32,
}

impl ForwardingTrack {
    pub fn new(
        track_id: TrackId,
        source_peer_track_id: TrackId,
        peer_sender: mpsc::Sender<AudioFrame>,
        inbound_receiver: mpsc::Receiver<AudioFrame>,
        config: TrackConfig,
        cancel_token: CancellationToken,
        ssrc: u32,
    ) -> Self {
        Self {
            processor_chain: ProcessorChain::new(config.samplerate),
            track_id,
            source_peer_track_id,
            peer_sender,
            inbound_receiver: Some(inbound_receiver),
            config,
            cancel_token,
            ssrc,
        }
    }
}

#[async_trait]
impl Track for ForwardingTrack {
    fn ssrc(&self) -> u32 {
        self.ssrc
    }

    fn id(&self) -> &TrackId {
        &self.track_id
    }

    fn config(&self) -> &TrackConfig {
        &self.config
    }

    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }

    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok(String::new())
    }

    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
    }

    async fn start(
        &mut self,
        _event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let mut inbound_receiver = self
            .inbound_receiver
            .take()
            .ok_or_else(|| anyhow::anyhow!("forwarding track already started"))?;
        let track_id = self.track_id.clone();
        let cancel_token = self.cancel_token.clone();

        crate::spawn(async move {
            let stop_reason = loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break "track stopped";
                    }
                    packet = inbound_receiver.recv() => {
                        match packet {
                            Some(mut packet) => {
                                packet.track_id = track_id.clone();
                                if packet_sender.send(packet).is_err() {
                                    break "media stream closed";
                                }
                            }
                            None => {
                                break "peer bridge channel closed";
                            }
                        }
                    }
                }
            };
            cancel_token.cancel();
            info!(track_id, reason = stop_reason, "audio bridge forwarding task stopped");
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&mut self, packet: &AudioFrame) -> Result<()> {
        if self.cancel_token.is_cancelled() || packet.track_id != self.source_peer_track_id {
            return Ok(());
        }

        if let Samples::RTP { payload_type, .. } = &packet.samples {
            if *payload_type >= 96 && *payload_type <= 127 {
                return Ok(());
            }
        }

        match self.peer_sender.try_send(packet.clone()) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.cancel_token.cancel();
            }
        }

        Ok(())
    }
}
