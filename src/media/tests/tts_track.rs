use crate::{
    event::SessionEvent,
    media::track::{Track, tts::TtsTrack},
    media::{Samples, cache},
    synthesis::{
        Subtitle, SynthesisClient, SynthesisCommand, SynthesisEvent, SynthesisOption, SynthesisType,
    },
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::BufMut;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::path::PathBuf;
use std::time::Instant;
use tokio::{
    sync::{broadcast, mpsc},
    time::Duration,
};
use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream};
use tokio_util::sync::CancellationToken;
use tracing::debug;

// A mock synthesis client that supports both streaming and non-streaming modes
struct MockSynthesisClient {
    // Channel for sending events back to the stream
    event_sender: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
    // Current mode (streaming vs non-streaming)
    streaming: bool,
}

impl MockSynthesisClient {
    fn new(streaming: bool) -> Self {
        Self {
            event_sender: None,
            streaming,
        }
    }

    // Generate a simple sine wave audio sample for testing
    fn generate_audio_sample(text: &str, sample_rate: u32) -> (Vec<u8>, u32) {
        let frequency = 440.0; // A4 note
        // Duration based on text length (roughly 100ms per character)
        let duration_seconds = (text.len() as f64 * 0.1).max(0.5).min(3.0);
        let num_samples = (sample_rate as f64 * duration_seconds) as usize;

        // Generate PCM audio data (16-bit)
        let mut audio_data = Vec::with_capacity(num_samples * 2);
        for i in 0..num_samples {
            let t = i as f64 / sample_rate as f64;
            let amplitude = 16384.0; // Half of 16-bit range (32768/2)
            let sample = (amplitude * (2.0 * std::f64::consts::PI * frequency * t).sin()) as i16;
            audio_data.put_i16_le(sample);
        }

        (audio_data, duration_seconds as u32)
    }

    // Generate subtitles for the given text
    fn generate_subtitles(text: &str, duration_ms: u32) -> Vec<Subtitle> {
        vec![Subtitle::new(
            text.to_string(),
            0,
            duration_ms,
            0,
            text.len() as u32,
        )]
    }

    // Send events to the stream
    async fn send_event(&self, cmd_seq: Option<usize>, event: SynthesisEvent) {
        if let Some(sender) = &self.event_sender {
            let _ = sender.send((cmd_seq, Ok(event)));
        }
    }
}

#[async_trait]
impl SynthesisClient for MockSynthesisClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Other("mock".to_string())
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_sender = Some(event_tx);
        Ok(UnboundedReceiverStream::new(event_rx).boxed())
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        let sample_rate = option
            .as_ref()
            .and_then(|opt| opt.samplerate)
            .unwrap_or(16000) as u32;

        // Generate audio data
        let (audio_data, duration_ms) = Self::generate_audio_sample(text, sample_rate);

        self.send_event(cmd_seq, SynthesisEvent::AudioChunk(Bytes::from(audio_data)))
            .await;
        self.send_event(
            cmd_seq,
            SynthesisEvent::Subtitles(Self::generate_subtitles(text, duration_ms)),
        )
        .await;
        if !self.streaming {
            self.send_event(cmd_seq, SynthesisEvent::Finished).await;
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(sender) = &self.event_sender {
            // sending finished for streaming mode
            let _ = sender.send((None, Ok(SynthesisEvent::Finished)));
        }
        self.event_sender.take();
        Ok(())
    }
}

#[tokio::test]
async fn test_tts_track_basic_non_streaming() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let non_streaming_client = MockSynthesisClient::new(false);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(non_streaming_client),
    );

    // Create channels for the test
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    // Wait for at least one audio frame
    let frame = packet_rx.recv().await.unwrap();
    // Verify the frame properties
    assert_eq!(frame.track_id, track_id, "Track ID mismatch");
    debug!("frame: {:?}", frame);
    // Check that we have PCM samples
    if let Samples::PCM { samples } = &frame.samples {
        assert!(samples.len() > 100, "Too few samples in the frame");
    } else {
        panic!("Expected PCM samples");
    }

    // Stop the track
    tts_track.stop().await?;
    let mut track_end = false;
    while let Ok(event) = event_rx.recv().await {
        if let SessionEvent::TrackEnd { .. } = event {
            track_end = true;
            break;
        }
    }
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_basic_streaming() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let streaming_client = MockSynthesisClient::new(true);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        true,
        None,
        command_rx,
        Box::new(streaming_client),
    );

    // Create channels for the test
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    // Wait for at least one audio frame
    let frame = packet_rx.recv().await.unwrap();
    // Verify the frame properties
    assert_eq!(frame.track_id, track_id, "Track ID mismatch");
    // Check that we have PCM samples
    if let Samples::PCM { samples } = &frame.samples {
        assert!(samples.len() > 100, "Too few samples in the frame");
    } else {
        panic!("Expected PCM samples");
    }

    // Stop the track
    tts_track.stop().await?;
    let mut track_end = false;
    while let Ok(event) = event_rx.recv().await {
        if let SessionEvent::TrackEnd { .. } = event {
            track_end = true;
            break;
        }
    }
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_multiple_commands_non_streaming() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track-multiple".to_string();
    let client = MockSynthesisClient::new(false);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_cache_enabled(false); // Disable caching for this test

    // Create channels for the test
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send multiple TTS commands
    for i in 1..=3 {
        command_tx.send(SynthesisCommand {
            text: format!("Test speech synthesis {}", i),
            play_id: Some(format!("test-{}", i)),
            ..Default::default()
        })?;
    }

    // Collect frames for a short period
    let timeout = Duration::from_secs(5);
    let mut frames = Vec::new();

    loop {
        match tokio::time::timeout(timeout, packet_rx.recv()).await {
            Ok(Some(frame)) => {
                frames.push(frame);
                if frames.len() >= 10 {
                    break; // Collected enough frames
                }
            }
            _ => break, // Either timeout or channel closed
        }
    }

    // Verify that we received multiple frames
    assert!(!frames.is_empty(), "Expected at least one audio frame");

    // Check that all frames have the correct track ID
    for frame in &frames {
        assert_eq!(frame.track_id, track_id, "Track ID mismatch");

        // Ensure each frame has valid PCM samples
        match &frame.samples {
            Samples::PCM { samples } => {
                assert!(!samples.is_empty(), "Expected non-empty samples");
            }
            _ => panic!("Expected PCM samples"),
        }
    }

    // Stop the track
    tts_track.stop().await?;

    Ok(())
}

#[tokio::test]
async fn test_tts_track_configuration() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with custom configuration
    let track_id = "test-track-config".to_string();
    let client = MockSynthesisClient::new(false);
    let custom_sample_rate = 8000; // Use 8kHz instead of default 16kHz
    let custom_ptime = Duration::from_millis(10); // Use 10ms packet time

    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_sample_rate(custom_sample_rate)
    .with_ptime(custom_ptime);

    // Create channels for the test
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test with custom configuration".to_string(),
        speaker: Some("test-speaker".to_string()),
        play_id: Some("config-test".to_string()),
        ..Default::default()
    })?;

    // Wait for an audio frame
    let timeout = Duration::from_secs(5);
    let result = tokio::time::timeout(timeout, packet_rx.recv()).await;

    // Verify the frame
    assert!(result.is_ok(), "Timed out waiting for audio frame");
    let frame = result.unwrap();
    assert!(frame.is_some(), "Expected audio frame, got None");

    let frame = frame.unwrap();

    // Verify the sample rate matches our configuration
    assert_eq!(frame.sample_rate, 16000, "Sample rate mismatch");

    // Stop the track
    tts_track.stop().await?;

    Ok(())
}

#[tokio::test]
async fn test_tts_track_interrupt() -> Result<()> {
    // Create a command channel
    let cancel_token = CancellationToken::new();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track".to_string();
    let client = MockSynthesisClient::new(false);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_cancel_token(cancel_token.clone());

    // Create channels for the test
    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, mut _packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    tokio::time::sleep(Duration::from_millis(50)).await;
    tts_track.stop().await?;
    // Wait for at least one audio frame
    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;
    let mut interrupted = false;
    let mut track_end = false;
    for result in results {
        match result {
            Ok(SessionEvent::Interruption { .. }) => {
                interrupted = true;
            }
            Ok(SessionEvent::TrackEnd { .. }) => {
                track_end = true;
            }
            _ => {}
        }
    }
    assert!(interrupted, "Track was not interrupted");
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_interrupt_graceful() -> Result<()> {
    // Create a command channel
    let cancel_token = CancellationToken::new();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track".to_string();
    let client = MockSynthesisClient::new(false);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_cancel_token(cancel_token.clone());

    // Create channels for the test
    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, mut _packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    tokio::time::sleep(Duration::from_millis(50)).await;
    let now = Instant::now();
    tts_track.stop_graceful().await?;
    // Wait for at least one audio frame
    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;
    let mut interrupted = false;
    let mut track_end = false;
    for result in results {
        match result {
            Ok(SessionEvent::Interruption { .. }) => {
                interrupted = true;
            }
            Ok(SessionEvent::TrackEnd { .. }) => {
                track_end = true;
            }
            _ => {}
        }
    }
    assert!(
        now.elapsed() > Duration::from_millis(100),
        "Track was interrupted too early"
    );
    assert!(interrupted, "Track was not interrupted");
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_end_of_stream() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let non_streaming_client = MockSynthesisClient::new(false);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(non_streaming_client),
    );

    // Create channels for the test
    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test".to_string(),
        end_of_stream: true,
        ..Default::default()
    })?;

    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let mut track_end = false;
    for result in results {
        match result {
            Ok(SessionEvent::TrackEnd { .. }) => {
                track_end = true;
            }
            _ => {}
        }
    }

    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_base64() -> Result<()> {
    let original_cache_dir = cache::get_cache_dir()?;
    let temp_dir = tempfile::tempdir()?;
    let test_cache_dir = temp_dir.path().join("mediacache");
    cache::set_cache_dir(test_cache_dir.to_str().unwrap())?;

    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let non_streaming_client = MockSynthesisClient::new(false);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(non_streaming_client),
    );

    // Create channels for the test
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    let base64_text = tokio::fs::read("fixtures/base64noise.txt").await?;
    // remove the last '\n'
    let text = String::from_utf8(base64_text[..base64_text.len() - 1].to_vec())?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text,
        base64: true,
        end_of_stream: true,
        ..Default::default()
    })?;

    let mut sample_received = 0;
    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            biased;
            packet = packet_rx.recv() => {
                match packet {
                    Some(packet) => {
                        if let Samples::PCM { samples } = &packet.samples {
                            sample_received += samples.len();
                        }
                    }
                    None => {
                        break
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(SessionEvent::TrackEnd { .. }) => {
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                    _ => {}
                }
            }
            _ = &mut timeout => {
                break;
            }


        }
    }

    assert!(sample_received >= 8000, "Not enough bytes");
    let leaked_cache_file = PathBuf::from(format!("{}.pcm", test_cache_dir.display()));
    assert!(
        !tokio::fs::try_exists(&leaked_cache_file).await?,
        "base64 playback should not create an empty-key cache file at {}",
        leaked_cache_file.display()
    );
    cache::set_cache_dir(original_cache_dir.to_str().unwrap())?;
    Ok(())
}

struct DelayedMockSynthesisClient {
    event_sender: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
    streaming: bool,
    chunk_delay_ms: u64,
}

impl DelayedMockSynthesisClient {
    fn new(streaming: bool, chunk_delay_ms: u64) -> Self {
        Self {
            event_sender: None,
            streaming,
            chunk_delay_ms,
        }
    }

    fn generate_audio(sample_rate: u32, duration_ms: u32) -> Vec<u8> {
        let num_samples = (sample_rate as u64 * duration_ms as u64 / 1000) as usize;
        let mut data = Vec::with_capacity(num_samples * 2);
        for i in 0..num_samples {
            let t = i as f64 / sample_rate as f64;
            let sample = (16384.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16;
            data.put_i16_le(sample);
        }
        data
    }
}

#[async_trait]
impl SynthesisClient for DelayedMockSynthesisClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Other("delayed_mock".to_string())
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.event_sender = Some(tx);
        Ok(UnboundedReceiverStream::new(rx).boxed())
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        let sample_rate = 16000u32;
        // Send audio in multiple small chunks to simulate real streaming
        let total_duration_ms = (text.len() as u32 * 100).max(500);
        let chunk_duration_ms = 100u32;
        let num_chunks = total_duration_ms / chunk_duration_ms;

        let sender = self.event_sender.as_ref().unwrap().clone();
        let delay = self.chunk_delay_ms;
        let streaming = self.streaming;

        tokio::spawn(async move {
            for _ in 0..num_chunks {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                let audio = Self::generate_audio(sample_rate, chunk_duration_ms);
                let _ = sender.send((cmd_seq, Ok(SynthesisEvent::AudioChunk(Bytes::from(audio)))));
            }
            // For non-streaming mode, send Finished per command
            if !streaming {
                let _ = sender.send((cmd_seq, Ok(SynthesisEvent::Finished)));
            }
        });

        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(sender) = &self.event_sender {
            let _ = sender.send((None, Ok(SynthesisEvent::Finished)));
        }
        self.event_sender.take();
        Ok(())
    }
}

#[tokio::test]
async fn test_non_streaming_multi_cmd_all_entries_finish() -> Result<()> {
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    let track_id = "test-autohang-nonstream".to_string();
    let client = DelayedMockSynthesisClient::new(false, 10);
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false, // non-streaming
        None,
        command_rx,
        Box::new(client),
    )
    .with_cache_enabled(false);

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    // Send multiple non-streaming commands (simulating sequential TTS segments)
    for i in 0..3 {
        command_tx.send(SynthesisCommand {
            text: format!("Segment {}", i),
            ..Default::default()
        })?;
    }
    // Mark end of stream
    command_tx.send(SynthesisCommand {
        text: String::new(),
        end_of_stream: true,
        ..Default::default()
    })?;

    // Wait for TrackEnd with a timeout
    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let track_end_received = results
        .iter()
        .any(|r| matches!(r, Ok(SessionEvent::TrackEnd { .. })));

    assert!(
        track_end_received,
        "Bug 1: TrackEnd not received in non-streaming mode with multiple commands. \
         This indicates emit_q entries were not properly marked as finished, \
         which would also prevent auto_hangup from triggering."
    );

    Ok(())
}

#[tokio::test]
async fn test_streaming_track_end_emitted_with_correct_ssrc() -> Result<()> {
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    let track_id = "test-streaming-trackend".to_string();
    let expected_ssrc: u32 = 12345;
    let client = DelayedMockSynthesisClient::new(true, 5); // fast chunks
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        true, // streaming
        Some("play-1".to_string()),
        command_rx,
        Box::new(client),
    )
    .with_ssrc(expected_ssrc)
    .with_cache_enabled(false);

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    // Send a streaming command and mark end_of_stream
    command_tx.send(SynthesisCommand {
        text: "Hello streaming world".to_string(),
        streaming: true,
        ..Default::default()
    })?;
    command_tx.send(SynthesisCommand {
        text: String::new(),
        end_of_stream: true,
        streaming: true,
        ..Default::default()
    })?;

    // Wait for TrackEnd
    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let mut track_end_received = false;
    let mut track_end_ssrc: Option<u32> = None;
    let mut track_end_play_id: Option<Option<String>> = None;

    for result in &results {
        if let Ok(SessionEvent::TrackEnd { ssrc, play_id, .. }) = result {
            track_end_received = true;
            track_end_ssrc = Some(*ssrc);
            track_end_play_id = Some(play_id.clone());
        }
    }

    assert!(
        track_end_received,
        "Bug 2: TrackEnd not received in streaming mode. \
         The streaming early-exit path (emit_q.clear) may be preventing TrackEnd emission."
    );

    assert_eq!(
        track_end_ssrc,
        Some(expected_ssrc),
        "TrackEnd SSRC mismatch: auto_hangup in ActiveCall::process() matches by SSRC"
    );

    assert_eq!(
        track_end_play_id,
        Some(Some("play-1".to_string())),
        "TrackEnd play_id mismatch: ActiveCall::process() filters TrackEnd by current_play_id"
    );

    Ok(())
}

#[tokio::test]
async fn test_streaming_finished_with_buffered_audio() -> Result<()> {
    // This mock sends Finished immediately before audio chunks arrive
    struct RaceMockClient {
        event_sender: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
    }

    #[async_trait]
    impl SynthesisClient for RaceMockClient {
        fn provider(&self) -> SynthesisType {
            SynthesisType::Other("race_mock".to_string())
        }

        async fn start(
            &mut self,
        ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
            let (tx, rx) = mpsc::unbounded_channel();
            self.event_sender = Some(tx);
            Ok(UnboundedReceiverStream::new(rx).boxed())
        }

        async fn synthesize(
            &mut self,
            _text: &str,
            _cmd_seq: Option<usize>,
            _option: Option<SynthesisOption>,
        ) -> Result<()> {
            // Do nothing - events are sent manually
            Ok(())
        }

        async fn stop(&mut self) -> Result<()> {
            self.event_sender.take();
            Ok(())
        }
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();

    let track_id = "test-race-streaming".to_string();
    let client = RaceMockClient { event_sender: None };
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        true, // streaming
        Some("play-race".to_string()),
        command_rx,
        Box::new(client),
    )
    .with_ssrc(99999)
    .with_cache_enabled(false);

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    // Send command with end_of_stream to trigger cmd_finished
    command_tx.send(SynthesisCommand {
        text: "Race test".to_string(),
        streaming: true,
        end_of_stream: true,
        ..Default::default()
    })?;

    let timeout = tokio::time::sleep(Duration::from_secs(8));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let track_end_received = results
        .iter()
        .any(|r| matches!(r, Ok(SessionEvent::TrackEnd { .. })));

    assert!(
        track_end_received,
        "Bug 2 edge case: TrackEnd not received when streaming TTS ends \
         with no audio data (synthesis provider sends nothing)."
    );

    Ok(())
}

#[tokio::test]
async fn test_streaming_tts_stuck_stream_produces_track_end() -> Result<()> {
    struct StuckStreamMock {
        event_sender: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
        // Extra sender keeps the channel alive so stream.next() never returns None
        _keep_alive: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
    }

    impl StuckStreamMock {
        fn new() -> Self {
            Self {
                event_sender: None,
                _keep_alive: None,
            }
        }
    }

    #[async_trait]
    impl SynthesisClient for StuckStreamMock {
        fn provider(&self) -> SynthesisType {
            SynthesisType::Other("stuck_mock".to_string())
        }

        async fn start(
            &mut self,
        ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
            let (tx, rx) = mpsc::unbounded_channel();
            self.event_sender = Some(tx.clone());
            self._keep_alive = Some(tx); // keep channel alive
            Ok(UnboundedReceiverStream::new(rx).boxed())
        }

        async fn synthesize(
            &mut self,
            _text: &str,
            _cmd_seq: Option<usize>,
            _option: Option<SynthesisOption>,
        ) -> Result<()> {
            Ok(())
        }

        async fn stop(&mut self) -> Result<()> {
            self.event_sender.take();
            Ok(())
        }
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let track_id = "test-stuck-stream".to_string();
    let expected_ssrc: u32 = 99999;

    let client = StuckStreamMock::new();
    let mut tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        true, // streaming
        Some("stuck-play".to_string()),
        command_rx,
        Box::new(client),
    )
    .with_ssrc(expected_ssrc)
    .with_cache_enabled(false);

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    // Send a streaming command with end_of_stream to trigger cmd_finished
    command_tx.send(SynthesisCommand {
        text: "Stuck test".to_string(),
        streaming: true,
        end_of_stream: true,
        ..Default::default()
    })?;

    let timeout = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let track_end_received = results
        .iter()
        .any(|r| matches!(r, Ok(SessionEvent::TrackEnd { .. })));

    assert!(
        track_end_received,
        "Bug 2 (stuck stream): TrackEnd not received within timeout. \
         The TtsTask is stuck because the stream never closes and emit_q is empty. \
         An escape timeout is needed for this case."
    );

    Ok(())
}
