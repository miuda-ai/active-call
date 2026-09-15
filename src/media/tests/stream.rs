use crate::media::processor::ProcessorChain;
use crate::media::recorder::RecorderOption;
use crate::media::track::TrackConfig;
use crate::{
    event::EventSender,
    media::AudioFrame,
    media::Samples,
    media::TrackId,
    media::{
        stream::MediaStreamBuilder,
        track::{Track, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tracing::warn;

pub struct TestTrack {
    id: TrackId,
    config: TrackConfig,
    sender: Option<TrackPacketSender>,
    processor_chain: ProcessorChain,
    received_packets: Arc<Mutex<Vec<AudioFrame>>>,
}

impl TestTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            config: TrackConfig::default(),
            sender: None,
            processor_chain: ProcessorChain::new(16000),
            received_packets: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Track for TestTrack {
    fn ssrc(&self) -> u32 {
        0 // Placeholder, as TestTrack does not use SSRC
    }
    fn id(&self) -> &TrackId {
        &self.id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }
    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
    }
    async fn start(
        &mut self,
        _event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Store the packet sender for later use
        self.sender = Some(packet_sender);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn send_packet(&mut self, packet: &AudioFrame) -> Result<()> {
        {
            let mut received = self.received_packets.lock().await;
            received.push(packet.clone());
        }

        // Clone and process the packet
        let mut packet_clone = packet.clone();

        // Apply processors to the packet
        if let Err(e) = self.processor_chain.process_frame(&mut packet_clone) {
            warn!("Error processing packet: {}", e);
        }

        if let Some(sender) = &self.sender {
            match sender.send(packet_clone) {
                Ok(_) => {}
                Err(e) => {
                    warn!("Failed to send packet: {}", e);
                }
            }
        }

        Ok(())
    }
}

/// Sink track that only records forwarded packets (no echo back into the stream).
struct CollectTrack {
    id: TrackId,
    config: TrackConfig,
    processor_chain: ProcessorChain,
    received: Arc<Mutex<Vec<AudioFrame>>>,
}

impl CollectTrack {
    fn new(id: TrackId, received: Arc<Mutex<Vec<AudioFrame>>>) -> Self {
        Self {
            id,
            config: TrackConfig::default(),
            processor_chain: ProcessorChain::new(16000),
            received,
        }
    }
}

#[async_trait]
impl Track for CollectTrack {
    fn ssrc(&self) -> u32 {
        0
    }
    fn id(&self) -> &TrackId {
        &self.id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }
    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
    }
    async fn start(
        &mut self,
        _event_sender: EventSender,
        _packet_sender: TrackPacketSender,
    ) -> Result<()> {
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
    async fn send_packet(&mut self, packet: &AudioFrame) -> Result<()> {
        self.received.lock().await.push(packet.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stream_add_track() {
        let event_sender = crate::event::create_event_sender();
        let stream = MediaStreamBuilder::new(event_sender).build();
        let track = Box::new(TestTrack::new("test1".to_string()));
        stream.update_track(track, None).await;
    }

    #[tokio::test]
    async fn test_stream_remove_track() {
        let event_sender = crate::event::create_event_sender();
        let stream = MediaStreamBuilder::new(event_sender.clone())
            .with_id("ms:test".to_string())
            .build();
        let track_id = "test1".to_string();
        stream
            .update_track(Box::new(TestTrack::new(track_id.clone())), None)
            .await;
        stream.remove_track(&track_id, false).await;
    }
}

#[tokio::test]
async fn test_media_stream_basic() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    // Add a test track
    let track = Box::new(TestTrack::new("test1".to_string()));

    stream.update_track(track, None).await;

    // Start the stream
    let handle = tokio::spawn(async move {
        stream.serve().await.unwrap();
    });

    // Wait a bit
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_media_stream_events() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender.clone()).build();

    let _events = event_sender.subscribe();

    // Add a test track
    let track = Box::new(TestTrack::new("test1".to_string()));

    stream.update_track(track, None).await;

    // Start the stream
    let handle = tokio::spawn(async move {
        stream.serve().await.unwrap();
    });

    // Wait a bit
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

// New test for track packet forwarding
#[tokio::test]
async fn test_stream_forward_packets() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    // Create two test tracks
    let track1 = TestTrack::new("test1".to_string());
    let track2 = TestTrack::new("test2".to_string());

    // Get the track ID for the test packet
    let track2_id = track2.id().clone();

    // Add tracks to the stream
    stream.update_track(Box::new(track1), None).await;
    stream.update_track(Box::new(track2), None).await;
    let packet_sender = stream.packet_sender.clone();

    // Start the stream in a background task
    let handle = tokio::spawn(async move {
        stream.serve().await.unwrap();
    });

    // Allow time for setup
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send PCM data through the sender
    let samples = vec![16000, 8000, 12000, 4000];
    let packet = AudioFrame {
        track_id: track2_id.clone(),
        timestamp: 1000,
        samples: Samples::PCM { samples: samples },
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };

    // Try to send the packet - ignore errors
    let _ = packet_sender.send(packet);

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

// Test for the Recorder functionality
#[tokio::test]
async fn test_stream_recorder() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    // Create a stream with recorder enabled

    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_recording.wav");
    let stream = Arc::new(
        MediaStreamBuilder::new(event_sender)
            .with_recorder_config(RecorderOption {
                recorder_file: file_path.to_string_lossy().to_string(),
                ..Default::default()
            })
            .build(),
    );

    // Create two test tracks
    let track1 = Box::new(TestTrack::new("test1".to_string()));
    let track2 = Box::new(TestTrack::new("test2".to_string()));

    // Get the track ID for the test packet
    let track2_id = track2.id().clone();

    // Add tracks to the stream
    stream.update_track(track1, None).await;
    stream.update_track(track2, None).await;

    // Clone the stream for the background task
    let stream_clone = stream.clone();

    // Start the stream in a background task
    let handle = tokio::spawn(async move {
        stream_clone.serve().await.unwrap();
    });

    // Allow time for setup
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Get access to the internal packet sender
    let packet_sender = stream.packet_sender.clone();

    // Send multiple PCM packets with different samples
    let samples1 = vec![3000, 6000, 9000, 12000];
    let samples2 = vec![15000, 18000, 21000, 24000];

    // Create the packets
    let packet1 = AudioFrame {
        track_id: track2_id.clone(),
        timestamp: 1000,
        samples: Samples::PCM { samples: samples1 },
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };

    let packet2 = AudioFrame {
        track_id: track2_id,
        timestamp: 1020,
        samples: Samples::PCM { samples: samples2 },
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };

    // Send the packets directly to the packet sender
    packet_sender.send(packet1).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    packet_sender.send(packet2).unwrap();

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

// Test for forwarding between different payload types
#[tokio::test]
async fn test_stream_forward_payload_conversion() -> Result<()> {
    // Create a stream
    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(MediaStreamBuilder::new(event_sender).build());

    // Create two test tracks with different packet types
    let track1 = TestTrack::new("track1".to_string()); // This will receive PCM
    let track2 = TestTrack::new("track2".to_string()); // This will send RTP

    // Add tracks to the stream
    stream.update_track(Box::new(track1), None).await;
    stream.update_track(Box::new(track2), None).await;

    // Start the stream in a background task
    let stream_clone = stream.clone();
    let handle = tokio::spawn(async move {
        stream_clone.serve().await.unwrap();
    });

    // Allow time for setup
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Get access to the internal packet sender
    let packet_sender = stream.packet_sender.clone();

    // Create an RTP packet from track2
    let rtp_packet = AudioFrame {
        track_id: "track2".to_string(),
        timestamp: 1000,
        samples: Samples::RTP {
            payload_type: 0,
            payload: vec![1, 2, 3, 4],
            sequence_number: 1,
        },
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };

    // Send the RTP packet - ignore errors
    let _ = packet_sender.send(rtp_packet);

    // Create a PCM packet from track1
    let pcm_packet = AudioFrame {
        track_id: "track1".to_string(),
        timestamp: 2000,
        samples: Samples::PCM {
            samples: vec![3000, 6000, 9000, 12000],
        },
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };

    // Send the PCM packet - ignore errors
    let _ = packet_sender.send(pcm_packet);

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_remove_processor() -> Result<()> {
    use crate::media::processor::Processor;

    // Define a test processor
    struct TestProcessor {
        #[allow(unused)]
        name: String,
    }

    impl Processor for TestProcessor {
        fn process_frame(&mut self, _frame: &mut AudioFrame) -> Result<()> {
            Ok(())
        }
    }

    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    // Create and add a track
    let track_id = "test-track".to_string();
    let mut track = TestTrack::new(track_id.clone());

    // Add processors to the track
    track
        .processor_chain
        .append_processor(Box::new(TestProcessor {
            name: "processor1".to_string(),
        }));
    track
        .processor_chain
        .append_processor(Box::new(TestProcessor {
            name: "processor2".to_string(),
        }));

    stream.update_track(Box::new(track), None).await;

    // Remove TestProcessor type
    let result = stream.remove_processor::<TestProcessor>(&track_id).await;
    assert!(result.is_ok());

    Ok(())
}

#[tokio::test]
async fn test_append_processor() -> Result<()> {
    use crate::media::processor::Processor;

    // Define a test processor
    struct AppendTestProcessor {
        _value: u32,
    }

    impl Processor for AppendTestProcessor {
        fn process_frame(&mut self, _frame: &mut AudioFrame) -> Result<()> {
            Ok(())
        }
    }

    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    // Create and add a track
    let track_id = "test-track".to_string();
    let track = TestTrack::new(track_id.clone());

    stream.update_track(Box::new(track), None).await;

    // Append a processor
    let processor = Box::new(AppendTestProcessor { _value: 42 });
    let result = stream.append_processor(&track_id, processor).await;
    assert!(result.is_ok());

    Ok(())
}

#[tokio::test]
async fn test_remove_processor_from_nonexistent_track() -> Result<()> {
    use crate::media::processor::Processor;

    struct NonexistentProcessor;

    impl Processor for NonexistentProcessor {
        fn process_frame(&mut self, _frame: &mut AudioFrame) -> Result<()> {
            Ok(())
        }
    }

    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    // Try to remove a processor from a track that doesn't exist
    let result = stream
        .remove_processor::<NonexistentProcessor>(&"nonexistent-track".to_string())
        .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));

    Ok(())
}

pub struct StoppableTestTrack {
    id: TrackId,
    config: TrackConfig,
    processor_chain: ProcessorChain,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

impl StoppableTestTrack {
    pub fn new(id: TrackId, stopped: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            id,
            config: TrackConfig::default(),
            processor_chain: ProcessorChain::new(16000),
            stopped,
        }
    }
}

#[async_trait]
impl Track for StoppableTestTrack {
    fn ssrc(&self) -> u32 {
        0
    }
    fn id(&self) -> &TrackId {
        &self.id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }
    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
    }
    async fn start(
        &mut self,
        _event_sender: EventSender,
        _packet_sender: TrackPacketSender,
    ) -> Result<()> {
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn send_packet(&mut self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_cleanup_drains_all_tracks() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender)
        .with_id("test-cleanup".to_string())
        .build();

    let stopped1 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped3 = Arc::new(std::sync::atomic::AtomicBool::new(false));

    stream
        .update_track(
            Box::new(StoppableTestTrack::new(
                "track1".to_string(),
                stopped1.clone(),
            )),
            None,
        )
        .await;
    stream
        .update_track(
            Box::new(StoppableTestTrack::new(
                "track2".to_string(),
                stopped2.clone(),
            )),
            None,
        )
        .await;
    stream
        .update_track(
            Box::new(StoppableTestTrack::new(
                "track3".to_string(),
                stopped3.clone(),
            )),
            None,
        )
        .await;

    // Verify tracks are present
    assert_eq!(stream.track_count().await, 3);

    // Cleanup should drain all tracks and call stop() on each
    stream.cleanup().await.unwrap();

    // All tracks should be stopped
    assert!(
        stopped1.load(std::sync::atomic::Ordering::SeqCst),
        "track1 should have been stopped"
    );
    assert!(
        stopped2.load(std::sync::atomic::Ordering::SeqCst),
        "track2 should have been stopped"
    );
    assert!(
        stopped3.load(std::sync::atomic::Ordering::SeqCst),
        "track3 should have been stopped"
    );

    // Tracks HashMap should be empty
    assert_eq!(
        stream.track_count().await,
        0,
        "all tracks should be drained after cleanup"
    );

    Ok(())
}

#[tokio::test]
async fn test_cleanup_is_idempotent() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender)
        .with_id("test-idempotent".to_string())
        .build();

    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    stream
        .update_track(
            Box::new(StoppableTestTrack::new(
                "track1".to_string(),
                stopped.clone(),
            )),
            None,
        )
        .await;

    // First cleanup
    stream.cleanup().await.unwrap();
    assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(stream.track_count().await, 0);

    // Second cleanup should be safe (no panic, no tracks to drain)
    stream.cleanup().await.unwrap();
    assert_eq!(stream.track_count().await, 0);

    Ok(())
}

#[tokio::test]
async fn test_ambiance_idle_fills_without_tts() -> Result<()> {
    use crate::media::INTERNAL_SAMPLERATE;
    use crate::media::ambiance::AmbianceOption;
    use crate::media::stream::SERVER_SIDE_TRACK_ID;
    use hound::{WavSpec, WavWriter};

    // Loud square wave so energy assertions are unambiguous.
    let dir = tempdir()?;
    let wav_path = dir.path().join("ambiance.wav");
    let spec = WavSpec {
        channels: 1,
        sample_rate: INTERNAL_SAMPLERATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = WavWriter::create(&wav_path, spec)?;
    for i in 0..INTERNAL_SAMPLERATE {
        writer.write_sample(if i % 2 == 0 { 8000i16 } else { -8000i16 })?;
    }
    writer.finalize()?;

    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(MediaStreamBuilder::new(event_sender).build());

    // Production order: serve first, then user track, then ambiance. Never create TTS.
    let serve_stream = stream.clone();
    let handle = tokio::spawn(async move {
        serve_stream.serve().await.ok();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let received = Arc::new(Mutex::new(Vec::new()));
    stream
        .update_track(
            Box::new(CollectTrack::new("user".to_string(), received.clone())),
            None,
        )
        .await;

    let option = AmbianceOption {
        path: Some(wav_path.to_string_lossy().to_string()),
        duck_level: Some(0.5),
        normal_level: Some(1.0),
        transition_speed: Some(1.0),
        enabled: Some(true),
    };
    stream
        .ensure_ambiance(option, SERVER_SIDE_TRACK_ID.to_string())
        .await?
        .expect("ambiance should load");

    tokio::time::sleep(Duration::from_millis(220)).await;
    stream.stop(None, None);
    handle.abort();

    let packets = received.lock().await;
    let pcm_packets: Vec<_> = packets
        .iter()
        .filter_map(|p| match &p.samples {
            Samples::PCM { samples } if samples.iter().any(|s| s.abs() > 1000) => Some(samples),
            _ => None,
        })
        .collect();

    assert!(
        pcm_packets.len() >= 8,
        "no-TTS idle should keep playing ambiance (~20ms); got {} energetic frames",
        pcm_packets.len()
    );
    for samples in &pcm_packets {
        assert_eq!(
            samples.len(),
            320,
            "idle ambiance frames must be 20ms @ 16kHz"
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_ambiance_idle_is_recorded_into_right_channel() -> Result<()> {
    use crate::media::stream::SERVER_SIDE_TRACK_ID;

    let wav_path = write_loud_ambiance_wav().await?;

    let rec_dir = tempdir()?;
    let file_path = rec_dir.path().join("recording.wav");
    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(
        MediaStreamBuilder::new(event_sender)
            .with_recorder_config(RecorderOption {
                recorder_file: file_path.to_string_lossy().to_string(),
                ..Default::default()
            })
            .build(),
    );

    // Production order: serve (starts recorder), user track, then ambiance. No TTS.
    let serve_stream = stream.clone();
    let handle = tokio::spawn(async move {
        serve_stream.serve().await.ok();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let received = Arc::new(Mutex::new(Vec::new()));
    stream
        .update_track(
            Box::new(CollectTrack::new("user".to_string(), received.clone())),
            None,
        )
        .await;

    stream
        .ensure_ambiance(
            ambiance_option(wav_path.to_str().unwrap()),
            SERVER_SIDE_TRACK_ID.to_string(),
        )
        .await?
        .expect("ambiance should load");

    // Let the idle loop play ~0.5s of background audio.
    tokio::time::sleep(Duration::from_millis(500)).await;
    stream.stop(None, None);
    handle.abort();
    // The recorder flushes buffered samples on cancellation; give it time
    // to finalize the wav file.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut reader = hound::WavReader::open(&file_path)?;
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 16000);

    // Interleaved stereo: even samples = left (caller), odd = right (server side).
    let mut right_loud = 0usize;
    let mut left_max = 0i32;
    for (i, s) in reader.samples::<i16>().enumerate() {
        let s = s? as i32;
        if i % 2 == 0 {
            left_max = left_max.max(s.abs());
        } else if s.abs() > 1000 {
            right_loud += 1;
        }
    }

    assert!(
        right_loud >= 3200,
        "idle ambiance must be recorded into the right channel; got {} loud samples (~{}ms)",
        right_loud,
        right_loud / 16
    );
    assert_eq!(
        left_max, 0,
        "left channel must stay silent without caller audio"
    );

    Ok(())
}

#[tokio::test]
async fn test_ambiance_idle_resumes_after_tts_stops() -> Result<()> {
    use crate::media::INTERNAL_SAMPLERATE;
    use crate::media::ambiance::AmbianceOption;
    use crate::media::stream::SERVER_SIDE_TRACK_ID;
    use hound::{WavSpec, WavWriter};

    let dir = tempdir()?;
    let wav_path = dir.path().join("ambiance.wav");
    let spec = WavSpec {
        channels: 1,
        sample_rate: INTERNAL_SAMPLERATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = WavWriter::create(&wav_path, spec)?;
    for i in 0..INTERNAL_SAMPLERATE {
        writer.write_sample(if i % 2 == 0 { 8000i16 } else { -8000i16 })?;
    }
    writer.finalize()?;

    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(MediaStreamBuilder::new(event_sender).build());
    let serve_stream = stream.clone();
    let handle = tokio::spawn(async move {
        serve_stream.serve().await.ok();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let received = Arc::new(Mutex::new(Vec::new()));
    stream
        .update_track(
            Box::new(CollectTrack::new("user".to_string(), received.clone())),
            None,
        )
        .await;
    stream
        .ensure_ambiance(
            AmbianceOption {
                path: Some(wav_path.to_string_lossy().to_string()),
                duck_level: Some(0.5),
                normal_level: Some(1.0),
                transition_speed: Some(1.0),
                enabled: Some(true),
            },
            SERVER_SIDE_TRACK_ID.to_string(),
        )
        .await?
        .expect("ambiance should load");

    // Let idle run, then simulate TTS frames on the server-side track.
    tokio::time::sleep(Duration::from_millis(80)).await;
    {
        received.lock().await.clear();
    }

    for _ in 0..5 {
        stream.packet_sender.send(AudioFrame {
            track_id: SERVER_SIDE_TRACK_ID.to_string(),
            samples: Samples::PCM {
                samples: vec![100; 320],
            },
            timestamp: crate::media::get_timestamp(),
            sample_rate: INTERNAL_SAMPLERATE,
            channels: 1,
            ..Default::default()
        })?;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // During TTS, idle filler must not keep pumping 8000-level ambiance alone.
    let during_tts = {
        let packets = received.lock().await;
        packets
            .iter()
            .filter(|p| match &p.samples {
                Samples::PCM { samples } => samples.iter().any(|s| s.abs() > 1000),
                _ => false,
            })
            .count()
    };
    assert!(
        during_tts <= 2,
        "idle ambiance should pause while TTS frames are flowing, got {} loud frames",
        during_tts
    );

    received.lock().await.clear();
    // After TTS stops, idle ambiance must resume on its own.
    tokio::time::sleep(Duration::from_millis(120)).await;
    stream.stop(None, None);
    handle.abort();

    let resumed = {
        let packets = received.lock().await;
        packets
            .iter()
            .filter(|p| match &p.samples {
                Samples::PCM { samples } => samples.iter().any(|s| s.abs() > 1000),
                _ => false,
            })
            .count()
    };
    assert!(
        resumed >= 3,
        "after TTS stops, idle ambiance must resume; got {} energetic frames",
        resumed
    );

    Ok(())
}

async fn write_loud_ambiance_wav() -> Result<std::path::PathBuf> {
    use crate::media::INTERNAL_SAMPLERATE;
    use hound::{WavSpec, WavWriter};

    let dir = tempdir()?;
    let wav_path = dir.path().join("ambiance.wav");
    let spec = WavSpec {
        channels: 1,
        sample_rate: INTERNAL_SAMPLERATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = WavWriter::create(&wav_path, spec)?;
    for i in 0..INTERNAL_SAMPLERATE {
        writer.write_sample(if i % 2 == 0 { 8000i16 } else { -8000i16 })?;
    }
    writer.finalize()?;
    // Persist the temp dir so the wav outlives this helper.
    Ok(dir.keep().join("ambiance.wav"))
}

fn ambiance_option(path: &str) -> crate::media::ambiance::AmbianceOption {
    crate::media::ambiance::AmbianceOption {
        path: Some(path.to_string()),
        duck_level: Some(0.5),
        normal_level: Some(1.0),
        transition_speed: Some(1.0),
        enabled: Some(true),
    }
}

#[tokio::test]
async fn test_ensure_ambiance_disabled_returns_none_and_stays_silent() -> Result<()> {
    use crate::media::stream::SERVER_SIDE_TRACK_ID;

    let wav_path = write_loud_ambiance_wav().await?;

    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(MediaStreamBuilder::new(event_sender).build());
    let serve_stream = stream.clone();
    let handle = tokio::spawn(async move {
        serve_stream.serve().await.ok();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let received = Arc::new(Mutex::new(Vec::new()));
    stream
        .update_track(
            Box::new(CollectTrack::new("user".to_string(), received.clone())),
            None,
        )
        .await;

    // Explicitly disabled.
    let mut option = ambiance_option(wav_path.to_str().unwrap());
    option.enabled = Some(false);
    assert!(
        stream
            .ensure_ambiance(option, SERVER_SIDE_TRACK_ID.to_string())
            .await?
            .is_none()
    );

    // No path at all.
    assert!(
        stream
            .ensure_ambiance(
                crate::media::ambiance::AmbianceOption::default(),
                SERVER_SIDE_TRACK_ID.to_string()
            )
            .await?
            .is_none()
    );

    tokio::time::sleep(Duration::from_millis(120)).await;
    stream.stop(None, None);
    handle.abort();

    let packets = received.lock().await;
    assert!(
        packets.is_empty(),
        "disabled ambiance must not emit any frame, got {}",
        packets.len()
    );

    Ok(())
}

#[tokio::test]
async fn test_ensure_ambiance_is_idempotent() -> Result<()> {
    use crate::media::stream::SERVER_SIDE_TRACK_ID;

    let wav_path = write_loud_ambiance_wav().await?;

    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(MediaStreamBuilder::new(event_sender).build());
    let serve_stream = stream.clone();
    let handle = tokio::spawn(async move {
        serve_stream.serve().await.ok();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let received = Arc::new(Mutex::new(Vec::new()));
    stream
        .update_track(
            Box::new(CollectTrack::new("user".to_string(), received.clone())),
            None,
        )
        .await;

    let first = stream
        .ensure_ambiance(
            ambiance_option(wav_path.to_str().unwrap()),
            SERVER_SIDE_TRACK_ID.to_string(),
        )
        .await?
        .expect("first call loads");
    let second = stream
        .ensure_ambiance(
            ambiance_option(wav_path.to_str().unwrap()),
            SERVER_SIDE_TRACK_ID.to_string(),
        )
        .await?
        .expect("second call returns existing");

    assert!(
        Arc::ptr_eq(&first, &second),
        "ensure_ambiance must reuse the loaded processor"
    );

    // Exactly one idle loop: ~50 fps for 200ms → at most ~15 frames (generous),
    // two loops would produce ~20+.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stream.stop(None, None);
    handle.abort();

    let packets = received.lock().await;
    let frame_count = packets.len();
    assert!(
        (4..=15).contains(&frame_count),
        "single idle loop expected (~10 frames in 200ms), got {}",
        frame_count
    );

    Ok(())
}

/// Forward-path throughput with the ambiance idle loop running vs not.
/// Follows the repo convention: skipped in debug builds (see perf_analysis.rs).
#[tokio::test]
async fn perf_forward_fanout_ambiance_on_vs_off() -> Result<()> {
    if cfg!(debug_assertions) {
        println!("Skipping forward fanout perf test in debug mode.");
        return Ok(());
    }
    use crate::media::INTERNAL_SAMPLERATE;
    use crate::media::stream::SERVER_SIDE_TRACK_ID;
    use std::time::Instant;

    const PACKETS: usize = 3000; // 60s of 20ms frames
    const PER_PACKET_BUDGET: Duration = Duration::from_micros(200);

    let wav_path = write_loud_ambiance_wav().await?;
    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(MediaStreamBuilder::new(event_sender).build());
    let serve_stream = stream.clone();
    let handle = tokio::spawn(async move {
        serve_stream.serve().await.ok();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let received_a = Arc::new(Mutex::new(Vec::new()));
    let received_b = Arc::new(Mutex::new(Vec::new()));
    stream
        .update_track(
            Box::new(CollectTrack::new("user-a".to_string(), received_a.clone())),
            None,
        )
        .await;
    stream
        .update_track(
            Box::new(CollectTrack::new("user-b".to_string(), received_b.clone())),
            None,
        )
        .await;

    let make_frame = |i: usize| AudioFrame {
        track_id: SERVER_SIDE_TRACK_ID.to_string(),
        samples: Samples::PCM {
            samples: vec![(i % 100) as i16; 320],
        },
        timestamp: crate::media::get_timestamp(),
        sample_rate: INTERNAL_SAMPLERATE,
        channels: 1,
        ..Default::default()
    };

    // Wait until `target` frames from the TTS source arrived at track `sink`.
    async fn drain(sink: Arc<Mutex<Vec<AudioFrame>>>, label: &str) -> std::time::Duration {
        let deadline = tokio::time::Duration::from_secs(30);
        let start = Instant::now();
        loop {
            let done = {
                let packets = sink.lock().await;
                packets
                    .iter()
                    .filter(|p| p.track_id == SERVER_SIDE_TRACK_ID)
                    .count()
                    >= PACKETS
            };
            if done {
                return start.elapsed();
            }
            if start.elapsed() > deadline {
                panic!("drain timeout for {}", label);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    // Phase 1: ambiance off (regression baseline).
    for i in 0..PACKETS {
        stream.packet_sender.send(make_frame(i))?;
    }
    let off = drain(received_a.clone(), "ambiance-off").await;
    // Phase 2: ambiance idle loop running while TTS keeps pumping.
    stream
        .ensure_ambiance(
            ambiance_option(wav_path.to_str().unwrap()),
            SERVER_SIDE_TRACK_ID.to_string(),
        )
        .await?
        .expect("ambiance loads");
    tokio::time::sleep(Duration::from_millis(50)).await;
    received_a.lock().await.clear();
    received_b.lock().await.clear();
    for i in 0..PACKETS {
        stream.packet_sender.send(make_frame(i))?;
    }
    let on = drain(received_a.clone(), "ambiance-on").await;

    let idle_frames_b = {
        let packets = received_b.lock().await;
        packets
            .iter()
            .filter(|p| p.track_id != SERVER_SIDE_TRACK_ID)
            .count()
    };

    stream.stop(None, None);
    handle.abort();

    let off_per = off.as_micros() as f64 / PACKETS as f64;
    let on_per = on.as_micros() as f64 / PACKETS as f64;
    println!(
        "forward fanout (2 sinks): ambiance-off {:.1} µs/packet, ambiance-on {:.1} µs/packet ({:.2}x), interleaved idle frames at second sink: {}",
        off_per,
        on_per,
        on.as_secs_f64() / off.as_secs_f64(),
        idle_frames_b
    );

    assert!(
        Duration::from_micros(on_per as u64) < PER_PACKET_BUDGET,
        "ambiance-on forward path {:.1} µs/packet exceeds {:.1} µs budget",
        on_per,
        PER_PACKET_BUDGET.as_micros() as f64
    );
    // TTS packets must fully suppress idle fill while flowing.
    assert!(
        idle_frames_b <= PACKETS / 100,
        "idle ambiance leaked {} frames while TTS was pumping",
        idle_frames_b
    );

    Ok(())
}

// End-to-end native-samplerate recording: tracks wired through the chain raw
// tap must produce a stereo WAV whose header records the detected native rate
// (8 kHz) while the 16 kHz callee leg gets resampled into the same file.
#[tokio::test]
async fn test_stream_recorder_native_samplerate() -> Result<()> {
    use crate::media::AudioFrame;

    let event_sender = crate::event::create_event_sender();
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("native_recording.wav");
    let stream = Arc::new(
        MediaStreamBuilder::new(event_sender)
            .with_id("caller-track".to_string())
            .with_recorder_config(RecorderOption {
                recorder_file: file_path.to_string_lossy().to_string(),
                native_samplerate: Some(true),
                ..Default::default()
            })
            .build(),
    );

    // The track whose id equals the stream id is the caller leg (channel 0).
    stream
        .update_track(Box::new(TestTrack::new("caller-track".to_string())), None)
        .await;
    stream
        .update_track(Box::new(TestTrack::new("callee-track".to_string())), None)
        .await;

    let stream_clone = stream.clone();
    let handle = tokio::spawn(async move {
        stream_clone.serve().await.ok();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let packet_sender = stream.packet_sender.clone();

    // Caller leg: 8 kHz PCM (e.g. decoded from PCMU).
    for i in 0..5 {
        let samples: Vec<i16> = (0..80) // 10ms @ 8kHz
            .map(|j| {
                let t = (i * 80 + j) as f32 / 8000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as i16
            })
            .collect();
        packet_sender
            .send(AudioFrame {
                track_id: "caller-track".to_string(),
                timestamp: (i * 10) as u64,
                samples: Samples::PCM { samples },
                sample_rate: 8000,
                channels: 1,
                ..Default::default()
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Callee leg: 16 kHz PCM (e.g. G722/TTS), must be resampled to 8 kHz.
    for i in 0..5 {
        let samples: Vec<i16> = (0..160) // 10ms @ 16kHz
            .map(|j| {
                let t = (i * 160 + j) as f32 / 16000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as i16
            })
            .collect();
        packet_sender
            .send(AudioFrame {
                track_id: "callee-track".to_string(),
                timestamp: (i * 10) as u64,
                samples: Samples::PCM { samples },
                sample_rate: 16000,
                channels: 1,
                ..Default::default()
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    stream.cleanup().await?;
    handle.abort();

    assert!(file_path.exists(), "recording file was not created");
    let reader = hound::WavReader::open(&file_path)?;
    let spec = reader.spec();
    assert_eq!(
        spec.sample_rate, 8000,
        "WAV header must record the native rate"
    );
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.bits_per_sample, 16);
    let frames = reader.len();
    assert!(frames > 0, "recording has no samples");
    println!(
        "stream native recording: {} frames ({:.0}ms @ 8kHz)",
        frames,
        frames as f64 * 1000.0 / 8000.0
    );

    Ok(())
}
