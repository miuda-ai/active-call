//! Track-level auto_hangup intent tests.
//!
//! The hangup intent ("hang up the call when this playback finishes") is
//! carried by the track itself and reported on its `TrackEnd` event:
//! - armed via the first command (`with_auto_hangup`) or a later command
//!   with `auto_hangup: Some(true)`
//! - reported on natural completion only; a cancelled (interrupted) track
//!   reports `None` so barge-in never triggers a spurious hangup.

use active_call::callrecord::CallRecordHangupReason;
use active_call::event::SessionEvent;
use active_call::media::track::Track;
use active_call::media::track::file::FileTrack;
use active_call::media::track::tts::TtsTrack;
use active_call::media::track::TrackConfig;
use active_call::synthesis::SynthesisCommand;
use active_call::synthesis::{SynthesisClient, SynthesisEvent, SynthesisType};
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream};

struct StreamMock {
    event_sender: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
}

#[async_trait]
impl SynthesisClient for StreamMock {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Other("stream_mock".to_string())
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
        _option: Option<active_call::synthesis::SynthesisOption>,
    ) -> Result<()> {
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

/// Run a TtsTrack to completion and return the auto_hangup carried by its TrackEnd.
async fn run_tts_track<F>(track_auto_hangup: Option<bool>, commands: F) -> Result<Option<CallRecordHangupReason>>
where
    F: FnOnce(mpsc::UnboundedSender<SynthesisCommand>),
{
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let mut tts_track = TtsTrack::new(
        "test-track".to_string(),
        "test_session".to_string(),
        true,
        Some("stream-play-id".to_string()),
        command_rx,
        Box::new(StreamMock { event_sender: None }),
    )
    .with_ssrc(54321)
    .with_auto_hangup(track_auto_hangup)
    .with_cache_enabled(false);

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;
    commands(command_tx);

    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let auto_hangup = results.iter().find_map(|r| match r {
        Ok(SessionEvent::TrackEnd { auto_hangup, .. }) => Some(auto_hangup.clone()),
        _ => None,
    });
    Ok(auto_hangup.flatten())
}

/// A command with `auto_hangup: Some(true)` arms the track; its natural
/// TrackEnd reports `Some(BySystem)`.
#[tokio::test]
async fn test_tts_auto_hangup_reported_on_natural_end() -> Result<()> {
    let auto_hangup = run_tts_track(None, |tx| {
        tx.send(SynthesisCommand {
            text: "bye".to_string(),
            streaming: true,
            end_of_stream: true,
            auto_hangup: Some(true),
            ..Default::default()
        })
        .ok();
    })
    .await?;

    assert_eq!(
        auto_hangup,
        Some(CallRecordHangupReason::BySystem),
        "natural TrackEnd must carry the armed hangup intent"
    );
    Ok(())
}

/// Without the flag, TrackEnd carries no hangup intent.
#[tokio::test]
async fn test_tts_no_auto_hangup_by_default() -> Result<()> {
    let auto_hangup = run_tts_track(None, |tx| {
        tx.send(SynthesisCommand {
            text: "hello".to_string(),
            streaming: true,
            end_of_stream: true,
            ..Default::default()
        })
        .ok();
    })
    .await?;

    assert_eq!(auto_hangup, None);
    Ok(())
}

/// Streaming scenario: the intent may be armed by the first command (track
/// config) while later commands (same play_id) do not repeat the flag; the
/// intent survives until the stream finishes naturally.
#[tokio::test]
async fn test_tts_auto_hangup_preserved_across_streaming_commands() -> Result<()> {
    let auto_hangup = run_tts_track(Some(true), |tx| {
        tx.send(SynthesisCommand {
            text: "chunk 1".to_string(),
            streaming: true,
            ..Default::default()
        })
        .ok();
        tx.send(SynthesisCommand {
            text: String::new(),
            streaming: true,
            end_of_stream: true,
            // no auto_hangup flag on the final chunk — intent must be preserved
            ..Default::default()
        })
        .ok();
    })
    .await?;

    assert_eq!(
        auto_hangup,
        Some(CallRecordHangupReason::BySystem),
        "intent armed by the first command must survive later commands without the flag"
    );
    Ok(())
}

/// A cancelled (interrupted) track voids the intent: barge-in must not
/// trigger a hangup, even when the intent was armed.
#[tokio::test]
async fn test_tts_cancelled_track_voids_auto_hangup() -> Result<()> {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let mut tts_track = TtsTrack::new(
        "test-track".to_string(),
        "test_session".to_string(),
        true,
        Some("interrupted-play".to_string()),
        command_rx,
        Box::new(StreamMock { event_sender: None }),
    )
    .with_ssrc(54321)
    .with_auto_hangup(Some(true))
    .with_cache_enabled(false);

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    command_tx.send(SynthesisCommand {
        text: "will be interrupted".to_string(),
        streaming: true,
        ..Default::default()
    })?;

    // Barge-in: cancel while playing.
    tts_track.stop().await?;

    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let auto_hangup = results.iter().find_map(|r| match r {
        Ok(SessionEvent::TrackEnd { auto_hangup, .. }) => Some(auto_hangup.clone()),
        _ => None,
    });

    assert_eq!(
        auto_hangup,
        Some(None),
        "cancelled track must not report hangup intent on TrackEnd"
    );
    Ok(())
}

fn create_test_wav_file(samples: u32) -> Result<(String, tempfile::TempDir)> {
    let temp_dir = tempfile::tempdir()?;
    let file_path = temp_dir.path().join("test.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&file_path, spec)?;
    for i in 0..samples {
        let sample = ((i as f32 * 0.05).sin() * 10000.0) as i16;
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok((file_path.to_str().unwrap().to_string(), temp_dir))
}

/// Run a FileTrack to natural completion and return the TrackEnd auto_hangup.
async fn run_file_track(
    auto_hangup: Option<bool>,
    cancel_after_first_packet: bool,
) -> Result<Option<CallRecordHangupReason>> {
    // Short file: 0.2s at 16kHz
    let (path, _temp) = create_test_wav_file(3200)?;

    let mut file_track = FileTrack::new("file-track".to_string())
        .with_path(path)
        .with_play_id(Some("file-play".to_string()))
        .with_auto_hangup(auto_hangup)
        .with_config(
            TrackConfig::default()
                .with_sample_rate(16000)
                .with_ptime(Duration::from_millis(10)),
        );

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    file_track.start(event_tx, packet_tx).await?;

    if cancel_after_first_packet {
        // Wait for playback to actually start, then interrupt.
        let _ = packet_rx.recv().await;
        file_track.stop().await?;
    }

    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let auto_hangup = results.iter().find_map(|r| match r {
        Ok(SessionEvent::TrackEnd { auto_hangup, .. }) => Some(auto_hangup.clone()),
        _ => None,
    });
    Ok(auto_hangup.flatten())
}

#[tokio::test]
async fn test_file_track_auto_hangup_on_natural_end() -> Result<()> {
    let auto_hangup = run_file_track(Some(true), false).await?;
    assert_eq!(
        auto_hangup,
        Some(CallRecordHangupReason::BySystem),
        "file playback finishing naturally must carry the hangup intent"
    );
    Ok(())
}

#[tokio::test]
async fn test_file_track_no_auto_hangup_by_default() -> Result<()> {
    let auto_hangup = run_file_track(None, false).await?;
    assert_eq!(auto_hangup, None);
    Ok(())
}

#[tokio::test]
async fn test_file_track_cancelled_voids_auto_hangup() -> Result<()> {
    let auto_hangup = run_file_track(Some(true), true).await?;
    assert_eq!(
        auto_hangup,
        None,
        "interrupted file playback must not carry the hangup intent"
    );
    Ok(())
}
