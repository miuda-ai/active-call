use active_call::media::stream::MediaStreamBuilder;
use active_call::media::track::{Track, TrackConfig};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::time::Duration;

struct MockTrack {
    id: String,
    config: TrackConfig,
    ms: Option<Arc<active_call::media::stream::MediaStream>>,
}

#[async_trait]
impl Track for MockTrack {
    fn ssrc(&self) -> u32 {
        0
    }
    fn id(&self) -> &String {
        &self.id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut active_call::media::processor::ProcessorChain {
        unimplemented!()
    }
    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        if let Some(ms) = &self.ms {
            // Recursive lock attempt
            let _ = ms
                .update_remote_description(&self.id, &"recursive".to_string())
                .await;
        }
        Ok(())
    }
    async fn start(
        &mut self,
        _event_sender: active_call::event::EventSender,
        _packet_sender: active_call::media::track::TrackPacketSender,
    ) -> Result<()> {
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
    async fn send_packet(&mut self, _packet: &active_call::media::AudioFrame) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_tracks_lock_deadlock() {
    let (event_sender, _) = tokio::sync::broadcast::channel(10);
    let ms = MediaStreamBuilder::new(event_sender.clone()).build();
    let ms = Arc::new(ms);

    let track_id = "test-track".to_string();
    let track = Box::new(MockTrack {
        id: track_id.clone(),
        config: TrackConfig::default(),
        ms: Some(ms.clone()),
    });

    ms.update_track(track, None).await;

    let ms_clone = ms.clone();
    let handle = tokio::spawn(async move {
        let _ = ms_clone
            .update_remote_description(&"test-track".to_string(), &"sdp".to_string())
            .await;
    });

    match tokio::time::timeout(Duration::from_secs(1), handle).await {
        Ok(_) => println!("Successfully completed (no deadlock)"),
        Err(_) => {
            println!("DEADLOCK DETECTED");
            panic!("Deadlock detected in MediaStream::tracks recursive locking");
        }
    }
}
