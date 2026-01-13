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

        tokio::spawn(async move {
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
