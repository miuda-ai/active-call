use crate::media::{
    AudioFrame, Samples,
    recorder::{Recorder, RecorderOption},
};
use anyhow::Result;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_recorder_rtp_g711() -> Result<()> {
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_g711.wav");
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test_g711".to_string(),
        config,
    ));

    let (tx, rx) = mpsc::unbounded_channel();
    let recorder_clone = recorder.clone();
    let file_path_clone = file_path.clone();

    let handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    // Send G.711 PCMU (PT=0) frames
    let payload = vec![0u8; 160]; // 20ms
    let frame = AudioFrame {
        track_id: "track1".to_string(),
        samples: Samples::RTP {
            sequence_number: 1,
            payload_type: 0,
            payload: payload.clone(),
        },
        timestamp: 0,
        sample_rate: 8000,
        channels: 1,
    };
    tx.send(frame)?;

    drop(tx);
    handle.await??;

    // Verify file exists and size
    // Header (44 bytes) + 160 bytes data = 204 bytes
    let metadata = std::fs::metadata(&file_path)?;
    assert_eq!(metadata.len(), 44 + 160);

    Ok(())
}

#[tokio::test]
async fn test_recorder_rtp_g722() -> Result<()> {
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_g722.wav");
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test_g722".to_string(),
        config,
    ));

    let (tx, rx) = mpsc::unbounded_channel();
    let recorder_clone = recorder.clone();
    let file_path_clone = file_path.clone();

    let handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    // Send G.722 (PT=9) frames
    let payload = vec![0u8; 160]; // 20ms
    let frame = AudioFrame {
        track_id: "track1".to_string(),
        samples: Samples::RTP {
            sequence_number: 1,
            payload_type: 9,
            payload: payload.clone(),
        },
        timestamp: 0,
        sample_rate: 16000,
        channels: 1,
    };
    tx.send(frame)?;

    drop(tx);
    handle.await??;

    // Verify file exists
    assert!(file_path.exists());
    let metadata = std::fs::metadata(&file_path)?;
    assert_eq!(metadata.len(), 44 + 160);

    Ok(())
}

#[tokio::test]
async fn test_recorder_rtp_pcma() -> Result<()> {
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_pcma.wav");
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test_pcma".to_string(),
        config,
    ));

    let (tx, rx) = mpsc::unbounded_channel();
    let recorder_clone = recorder.clone();
    let file_path_clone = file_path.clone();

    let handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    // Send PCMA (PT=8) frames
    let payload = vec![0u8; 160]; // 20ms
    let frame = AudioFrame {
        track_id: "track1".to_string(),
        samples: Samples::RTP {
            sequence_number: 1,
            payload_type: 8,
            payload: payload.clone(),
        },
        timestamp: 0,
        sample_rate: 8000,
        channels: 1,
    };
    tx.send(frame)?;

    drop(tx);
    handle.await??;

    // Verify file exists
    assert!(file_path.exists());
    let metadata = std::fs::metadata(&file_path)?;
    assert_eq!(metadata.len(), 44 + 160);

    Ok(())
}

#[tokio::test]
async fn test_recorder_rtp_pcm_l16() -> Result<()> {
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_pcm.wav");
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test_pcm".to_string(),
        config,
    ));

    let (tx, rx) = mpsc::unbounded_channel();
    let recorder_clone = recorder.clone();
    let file_path_clone = file_path.clone();

    let handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    // Send L16 Mono (PT=11) frames
    // 20ms at 44100Hz is 882 samples. 16-bit means 1764 bytes.
    let payload = vec![0u8; 1764];
    let frame = AudioFrame {
        track_id: "track1".to_string(),
        samples: Samples::RTP {
            sequence_number: 1,
            payload_type: 11,
            payload: payload.clone(),
        },
        timestamp: 0,
        sample_rate: 44100,
        channels: 1,
    };
    tx.send(frame)?;

    drop(tx);
    handle.await??;

    // Verify file exists
    assert!(file_path.exists());
    let metadata = std::fs::metadata(&file_path)?;
    assert_eq!(metadata.len(), 44 + 1764);

    Ok(())
}
