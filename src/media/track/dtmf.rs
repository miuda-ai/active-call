use crate::event::{EventSender, SessionEvent};
use crate::media::processor::ProcessorChain;
use crate::media::track::{Track, TrackConfig, TrackPacketSender};
use crate::media::{AudioFrame, Samples, TrackId};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use tokio::select;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub const DTMF_TRACK_ID: &str = "dtmf-track";
pub const DEFAULT_DTMF_DURATION_MS: u32 = 100;

/// Map a DTMF character to its RFC 4733 event code.
pub fn dtmf_event_code(c: char) -> Option<u8> {
    audio_codec::telephone_event::digit_to_event(c)
}

/// Producer track that plays one DTMF digit as `Samples::Dtmf` event
/// updates, one per ptime. Transport tracks map the events onto their own
/// wire format (e.g. RFC 4733 telephone-event for RTP).
pub struct DtmfTrack {
    track_id: TrackId,
    ssrc: u32,
    config: TrackConfig,
    processor_chain: ProcessorChain,
    event: u8,
    duration: Duration,
    play_id: Option<String>,
    cancel_token: CancellationToken,
}

impl DtmfTrack {
    pub fn new(track_id: TrackId, digit: char) -> Result<Self> {
        let event =
            dtmf_event_code(digit).ok_or_else(|| anyhow!("invalid DTMF digit: {:?}", digit))?;
        let config = TrackConfig::default();
        Ok(Self {
            track_id,
            ssrc: 0,
            processor_chain: ProcessorChain::new(config.samplerate),
            config,
            event,
            duration: Duration::from_millis(DEFAULT_DTMF_DURATION_MS as u64),
            play_id: None,
            cancel_token: CancellationToken::new(),
        })
    }

    pub fn with_ssrc(mut self, ssrc: u32) -> Self {
        self.ssrc = ssrc;
        self
    }

    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    pub fn with_play_id(mut self, play_id: Option<String>) -> Self {
        self.play_id = play_id;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }
}

#[async_trait]
impl Track for DtmfTrack {
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
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let track_id = self.track_id.clone();
        let ssrc = self.ssrc;
        let play_id = self.play_id.clone();
        let event = self.event;
        let ptime = self.config.ptime.max(Duration::from_millis(1));
        let duration = self.duration.max(ptime);
        let token = self.cancel_token.clone();
        let start_time = crate::media::get_timestamp();

        event_sender
            .send(SessionEvent::TrackStart {
                track_id: track_id.clone(),
                timestamp: start_time,
                play_id: play_id.clone(),
            })
            .ok();

        crate::spawn(async move {
            let send = |elapsed: Duration, end: bool| {
                packet_sender.send(AudioFrame {
                    track_id: track_id.clone(),
                    samples: Samples::Dtmf {
                        event,
                        duration_ms: elapsed.as_millis() as u32,
                        end,
                    },
                    timestamp: crate::media::get_timestamp(),
                    ..Default::default()
                })
            };

            // The first tick fires immediately, starting the event.
            let mut ticker = tokio::time::interval(ptime);
            let mut elapsed = Duration::ZERO;
            loop {
                select! {
                    _ = token.cancelled() => {
                        // Never leave the receiver with an open event.
                        if !elapsed.is_zero() {
                            send(elapsed, true).ok();
                        }
                        break;
                    }
                    _ = ticker.tick() => {}
                }
                elapsed = (elapsed + ptime).min(duration);
                let end = elapsed >= duration;
                if send(elapsed, end).is_err() {
                    debug!(track_id, "dtmf track: media stream closed");
                    break;
                }
                if end {
                    break;
                }
            }

            info!(track_id, play_id, "dtmf track finished");
            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::media::get_timestamp(),
                    duration: crate::media::get_timestamp() - start_time,
                    ssrc,
                    play_id,
                    auto_hangup: None,
                })
                .ok();
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&mut self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{broadcast, mpsc};

    fn collect_updates(frames: &[AudioFrame]) -> Vec<(u8, u32, bool)> {
        frames
            .iter()
            .filter_map(|f| match f.samples {
                Samples::Dtmf {
                    event,
                    duration_ms,
                    end,
                } => Some((event, duration_ms, end)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_dtmf_event_code() {
        assert_eq!(dtmf_event_code('0'), Some(0));
        assert_eq!(dtmf_event_code('9'), Some(9));
        assert_eq!(dtmf_event_code('*'), Some(10));
        assert_eq!(dtmf_event_code('#'), Some(11));
        assert_eq!(dtmf_event_code('A'), Some(12));
        assert_eq!(dtmf_event_code('d'), Some(15));
        assert_eq!(dtmf_event_code('x'), None);
        assert!(DtmfTrack::new("t".into(), 'x').is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn test_dtmf_track_emits_growing_event_updates() {
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
        let mut track = DtmfTrack::new("dtmf".into(), '5')
            .unwrap()
            .with_duration(Duration::from_millis(60))
            .with_play_id(Some("p1".into()));
        track.start(event_tx, packet_tx).await.unwrap();

        let mut frames = Vec::new();
        while let Some(frame) = packet_rx.recv().await {
            frames.push(frame);
        }
        assert_eq!(
            collect_updates(&frames),
            vec![(5, 20, false), (5, 40, false), (5, 60, true)]
        );

        assert!(matches!(
            event_rx.recv().await.unwrap(),
            SessionEvent::TrackStart { play_id: Some(ref p), .. } if p == "p1"
        ));
        assert!(matches!(
            event_rx.recv().await.unwrap(),
            SessionEvent::TrackEnd { play_id: Some(ref p), .. } if p == "p1"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn test_dtmf_track_cancel_closes_open_event() {
        let (event_tx, _event_rx) = broadcast::channel(16);
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let mut track = DtmfTrack::new("dtmf".into(), '7')
            .unwrap()
            .with_duration(Duration::from_millis(200))
            .with_cancel_token(token.clone());
        track.start(event_tx, packet_tx).await.unwrap();

        let first = packet_rx.recv().await.unwrap();
        assert_eq!(collect_updates(&[first]), vec![(7, 20, false)]);
        token.cancel();

        let mut frames = Vec::new();
        while let Some(frame) = packet_rx.recv().await {
            frames.push(frame);
        }
        let updates = collect_updates(&frames);
        let last = updates.last().copied().expect("end update on cancel");
        assert_eq!((last.0, last.2), (7, true));
        assert!(last.1 < 200);
    }
}
