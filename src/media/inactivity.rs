use super::processor::Processor;
use crate::event::{EventSender, SessionEvent};
use crate::media::{AudioFrame, get_timestamp};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct InactivityProcessor {
    last_received: Arc<AtomicU64>,
}

impl InactivityProcessor {
    pub fn new(
        track_id: String,
        timeout: Duration,
        event_sender: EventSender,
        cancel_token: CancellationToken,
    ) -> Self {
        let last_received = Arc::new(AtomicU64::new(get_timestamp()));
        let last_received_clone = last_received.clone();
        let track_id_clone = track_id.clone();

        crate::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    _ = interval.tick() => {
                        let last = last_received_clone.load(Ordering::SeqCst);
                        let now = get_timestamp();
                        if now > last && now - last > timeout.as_millis() as u64 {
                            info!(track_id = track_id_clone, "Inactivity timeout reached, sending inactivity event");
                            let _ = event_sender.send(SessionEvent::Inactivity {
                                track_id: track_id_clone.clone(),
                                timestamp: now,
                            });
                            break;
                        }
                    }
                }
            }
        });

        Self { last_received }
    }
}

impl Processor for InactivityProcessor {
    fn process_frame(&mut self, _frame: &mut AudioFrame) -> Result<()> {
        self.last_received.store(get_timestamp(), Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SessionEvent;

    fn frame(track_id: &str) -> AudioFrame {
        AudioFrame {
            track_id: track_id.to_string(),
            samples: crate::media::Samples::PCM { samples: vec![] },
            src_packet: None,
            timestamp: 0,
            sample_rate: 8000,
            channels: 1,
            speech_probability: None,
        }
    }

    #[tokio::test]
    async fn watchdog_fires_without_frames() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let token = CancellationToken::new();
        let _p = InactivityProcessor::new("t".to_string(), Duration::from_millis(120), tx, token);
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("no inactivity event within 2s");
        assert!(matches!(ev, Ok(SessionEvent::Inactivity { .. })));
    }

    /// Regression: the watchdog must be disarmed when its track's lifecycle
    /// token is cancelled (Track::stop / track replacement). Bound to the
    /// session token instead, a retired leg leaves a "zombie" watchdog whose
    /// frozen last-frame timestamp fires ~timeout later and hangs up a
    /// healthy call (INVITE-mode auto-refer transfer bug).
    #[tokio::test]
    async fn watchdog_disarmed_when_track_token_cancelled() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let session_token = CancellationToken::new();
        let track_token = session_token.child_token();
        let _p = InactivityProcessor::new(
            "t".to_string(),
            Duration::from_millis(120),
            tx,
            track_token,
        );
        session_token.cancel(); // session stays alive; only the track stopped
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            rx.try_recv().is_err(),
            "zombie watchdog fired after its track was stopped"
        );
    }

    #[tokio::test]
    async fn watchdog_reset_by_frames() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let token = CancellationToken::new();
        let mut p = InactivityProcessor::new("t".to_string(), Duration::from_millis(150), tx, token);
        // Feed frames for ~500ms, well past the 150ms timeout.
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            p.process_frame(&mut frame("t")).expect("process_frame");
        }
        assert!(
            rx.try_recv().is_err(),
            "watchdog fired despite continuous frames"
        );
    }
}
