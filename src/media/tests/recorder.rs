use crate::media::{
    AudioFrame, PcmBuf, Sample, Samples,
    recorder::{Recorder, RecorderOption},
};
use anyhow::Result;
use std::{path::Path, sync::Arc};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn verify_wav_file_with_rate(path: &Path, expected_rate: u32) -> Result<u32> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    assert_eq!(spec.channels, 2, "expected stereo recording");
    assert_eq!(spec.sample_rate, expected_rate, "unexpected sample rate");
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);
    let len = reader.len();
    assert!(len > 0, "WAV file has no samples");
    Ok(len)
}

#[tokio::test]
async fn test_recorder() -> Result<()> {
    // Setup
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_recording.wav");
    let file_path_clone = file_path.clone(); // Clone for the spawned task
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test".to_string(),
        config,
    ));

    // Create channels for testing
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording in the background
    let recorder_clone = recorder.clone();
    let recorder_hanadle = tokio::spawn(async move {
        let r = recorder_clone.process_recording(&file_path_clone, rx).await;
        println!("recorder: {:?}", r);
    });

    // Create test frames
    let left_channel_id = "left".to_string();
    let right_channel_id = "right".to_string();

    // Generate some sample audio data (sine wave)
    let sample_count = 1600; // 100ms of audio at 16kHz

    for i in 0..5 {
        // Send 5 frames (500ms total)
        // Left channel (lower frequency sine wave)
        let left_samples: PcmBuf = (0..sample_count)
            .map(|j| {
                let t = (i * sample_count + j) as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();

        // Right channel (higher frequency sine wave)
        let right_samples: PcmBuf = (0..sample_count)
            .map(|j| {
                let t = (i * sample_count + j) as f32 / 16000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();

        let left_frame = AudioFrame {
            track_id: left_channel_id.clone(),
            samples: Samples::PCM {
                samples: left_samples,
            },
            timestamp: (i * 100), // Increment timestamp by 100ms
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        };

        let right_frame = AudioFrame {
            track_id: right_channel_id.clone(),
            samples: Samples::PCM {
                samples: right_samples,
            },
            timestamp: (i * 100), // Same timestamp for synchronized channels
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        };

        // Send frames
        tx.send(left_frame)?;
        tx.send(right_frame)?;

        // Wait a bit to simulate real-time recording
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    recorder.stop_recording()?;
    recorder_hanadle.await?;
    // Verify the file exists
    assert!(file_path.exists());
    println!("file_path: {:?}", file_path.to_str());
    // Verify the file is a valid WAV file with expected content
    verify_wav_file(&file_path)?;

    Ok(())
}

fn verify_wav_file(path: &Path) -> Result<()> {
    // Open the WAV file
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();

    // Verify format
    assert_eq!(spec.channels, 2); // Stereo
    assert_eq!(spec.sample_rate, 16000);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);

    // Verify the file has some samples
    let samples_count = reader.len();
    assert!(samples_count > 0, "WAV file has no samples");

    Ok(())
}

#[tokio::test]
async fn test_recorder_intermittent_data() -> Result<()> {
    // Test for intermittent data handling and clipping detection
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_intermittent.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test".to_string(),
        config,
    ));
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording
    let recorder_clone = recorder.clone();
    let recorder_handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    let track_id = "test_track".to_string();

    // Test 1: Send intermittent small frames (less than chunk_size)
    for i in 0..10 {
        let small_samples: PcmBuf = (0..50) // Small frame, much less than chunk_size (320)
            .map(|j| {
                let t = (i * 50 + j) as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();

        let frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM {
                samples: small_samples,
            },
            timestamp: (i * 50) as u64,
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        };

        tx.send(frame)?;

        // Simulate intermittent arrival with gaps
        if i % 3 == 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    }

    // Test 2: Send frames with clipping values
    let clipped_samples: PcmBuf = vec![32767, -32768, 32767, -32768, 0, 0, 0, 0]; // Clipped values
    let clipped_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: clipped_samples,
        },
        timestamp: 1000,
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };
    tx.send(clipped_frame)?;

    // Test 3: Send frames with constant values (freeze detection)
    let constant_samples: PcmBuf = vec![1000; 20]; // 20 identical values
    let constant_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: constant_samples,
        },
        timestamp: 1100,
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };
    tx.send(constant_frame)?;

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Stop recording
    recorder.stop_recording()?;
    recorder_handle.await??;

    // Verify file exists and is valid
    assert!(file_path.exists());
    verify_wav_file(&file_path)?;

    println!("Intermittent data test completed successfully");
    Ok(())
}

#[tokio::test]
async fn test_constant_value_detection() -> Result<()> {
    // Test the improved constant value detection logic
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_constant_detection.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test".to_string(),
        config,
    ));
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording
    let recorder_clone = recorder.clone();
    let recorder_handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    let track_id = "test_track".to_string();

    // Test 1: Short silence (should NOT trigger warning)
    let short_silence: PcmBuf = vec![0; 15]; // Less than 20 samples
    let frame1 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: short_silence,
        },
        timestamp: 0,
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };
    tx.send(frame1)?;

    // Test 2: Medium silence (should NOT trigger warning as it's normal)
    let medium_silence: PcmBuf = vec![0; 40]; // Less than 50 samples
    let frame2 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: medium_silence,
        },
        timestamp: 100,
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };
    tx.send(frame2)?;

    // Test 3: Large silence buffer (should trigger warning)
    let large_silence: PcmBuf = vec![0; 100]; // More than 50 samples, all zeros
    let frame3 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: large_silence,
        },
        timestamp: 200,
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };
    tx.send(frame3)?;

    // Test 4: Non-zero constant values (should trigger warning)
    let constant_non_zero: PcmBuf = vec![1000; 50]; // 50 identical non-zero values
    let frame4 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: constant_non_zero,
        },
        timestamp: 300,
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };
    tx.send(frame4)?;

    // Test 5: Normal audio (should NOT trigger warning)
    let normal_audio: PcmBuf = (0..50)
        .map(|i| ((i as f32 * 0.1).sin() * 1000.0) as Sample)
        .collect();
    let frame5 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: normal_audio,
        },
        timestamp: 400,
        sample_rate: 16000,
        channels: 1,
        ..Default::default()
    };
    tx.send(frame5)?;

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Stop recording
    recorder.stop_recording()?;
    recorder_handle.await??;

    // Verify file exists and is valid
    assert!(file_path.exists());
    verify_wav_file(&file_path)?;

    println!("Constant value detection test completed successfully");
    Ok(())
}

#[tokio::test]
async fn test_recorder_200ms_timing() -> Result<()> {
    // Test with the new 200ms timing and improved buffer handling
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_200ms_recording.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default(); // Uses 200ms ptime now

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "test".to_string(),
        config,
    ));
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording
    let recorder_clone = recorder.clone();
    let recorder_handle = tokio::spawn(async move {
        let r = recorder_clone.process_recording(&file_path_clone, rx).await;
        println!("recorder result: {:?}", r);
    });

    let track_id_1 = "track_1".to_string();
    let track_id_2 = "track_2".to_string();

    // Send smaller, irregular frames to test the improved pop mechanism
    for i in 0..10 {
        // Varying frame sizes to test the new buffer handling
        let frame_size = 100 + (i * 50); // Variable frame sizes: 100, 150, 200, ..., 550

        let samples_1: PcmBuf = (0..frame_size)
            .map(|j| {
                let t = (i * frame_size + j) as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 8000.0) as Sample
            })
            .collect();

        let samples_2: PcmBuf = (0..frame_size)
            .map(|j| {
                let t = (i * frame_size + j) as f32 / 16000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 8000.0) as Sample
            })
            .collect();

        let frame_1 = AudioFrame {
            track_id: track_id_1.clone(),
            samples: Samples::PCM { samples: samples_1 },
            timestamp: (i * 200) as u64, // 200ms intervals
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        };

        let frame_2 = AudioFrame {
            track_id: track_id_2.clone(),
            samples: Samples::PCM { samples: samples_2 },
            timestamp: (i * 200) as u64,
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        };

        tx.send(frame_1)?;
        // Send second frame with slight delay to test buffer handling
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        tx.send(frame_2)?;

        // Wait for the 200ms interval
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    }

    // Let it process for a bit longer
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Stop recording - this should trigger the buffer flush
    recorder.stop_recording()?;
    recorder_handle.await?;

    // Verify the file exists and is valid
    assert!(file_path.exists());
    verify_wav_file(&file_path)?;

    println!("200ms timing test completed successfully");
    Ok(())
}

#[tokio::test]
async fn test_recorder_native_samplerate_8k() -> Result<()> {
    // Native-samplerate mode: both legs decoded from PCMU (8 kHz). The WAV
    // header must record 8 kHz instead of the 16 kHz pipeline rate.
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_native_8k.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption {
        native_samplerate: Some(true),
        ..Default::default()
    };

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "caller".to_string(),
        config,
    ));
    let (tx, rx) = mpsc::unbounded_channel();

    let recorder_clone = recorder.clone();
    let recorder_handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    for i in 0..6 {
        let caller_samples: PcmBuf = (0..80) // 10ms @ 8kHz
            .map(|j| {
                let t = (i * 80 + j) as f32 / 8000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();
        let callee_samples: PcmBuf = (0..80)
            .map(|j| {
                let t = (i * 80 + j) as f32 / 8000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();

        tx.send(AudioFrame {
            track_id: "caller".to_string(),
            samples: Samples::PCM {
                samples: caller_samples,
            },
            timestamp: (i * 10) as u64,
            sample_rate: 8000,
            channels: 1,
            ..Default::default()
        })?;
        tx.send(AudioFrame {
            track_id: "callee".to_string(),
            samples: Samples::PCM {
                samples: callee_samples,
            },
            timestamp: (i * 10) as u64,
            sample_rate: 8000,
            channels: 1,
            ..Default::default()
        })?;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    recorder.stop_recording()?;
    recorder_handle.await??;

    assert!(file_path.exists());
    let frames = verify_wav_file_with_rate(&file_path, 8000)?;
    let duration_ms = frames as f64 * 1000.0 / 8000.0;
    assert!(
        (40.0..=3000.0).contains(&duration_ms),
        "unexpected recording duration: {}ms",
        duration_ms
    );
    println!(
        "native 8k recording: {} frames ({:.0}ms)",
        frames, duration_ms
    );
    Ok(())
}

#[tokio::test]
async fn test_recorder_native_samplerate_mixed_rates() -> Result<()> {
    // Mixed rates: callee leg at 16 kHz (e.g. G722/TTS) while the caller leg
    // is 8 kHz (PCMU). Callee frames arriving before the first caller frame
    // must be buffered, then resampled to the detected 8 kHz target.
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_native_mixed.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption {
        native_samplerate: Some(true),
        ..Default::default()
    };

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "caller".to_string(),
        config,
    ));
    let (tx, rx) = mpsc::unbounded_channel();

    let recorder_clone = recorder.clone();
    let recorder_handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    // Callee frames (16 kHz) arrive first: they go to the pending buffer.
    for i in 0..3 {
        let callee_samples: PcmBuf = (0..160) // 10ms @ 16kHz
            .map(|j| {
                let t = (i * 160 + j) as f32 / 16000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();
        tx.send(AudioFrame {
            track_id: "callee".to_string(),
            samples: Samples::PCM {
                samples: callee_samples,
            },
            timestamp: (i * 10) as u64,
            sample_rate: 16000,
            channels: 1,
            ..Default::default()
        })?;
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }

    // First caller frame (8 kHz) latches the native target rate and flushes
    // the pending callee frames through the resampler.
    for i in 0..4 {
        let caller_samples: PcmBuf = (0..160) // 20ms @ 8kHz
            .map(|j| {
                let t = (i * 160 + j) as f32 / 8000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();
        tx.send(AudioFrame {
            track_id: "caller".to_string(),
            samples: Samples::PCM {
                samples: caller_samples,
            },
            timestamp: (i * 20) as u64,
            sample_rate: 8000,
            channels: 1,
            ..Default::default()
        })?;
        tokio::time::sleep(tokio::time::Duration::from_millis(60)).await;
    }

    recorder.stop_recording()?;
    recorder_handle.await??;

    assert!(file_path.exists());
    let frames = verify_wav_file_with_rate(&file_path, 8000)?;
    println!("native mixed-rate recording: {} frames", frames);
    Ok(())
}

#[tokio::test]
async fn test_processor_chain_raw_tap_keeps_native_rate() -> Result<()> {
    // The raw tap must mirror the frame at its native rate (8 kHz for PCMU)
    // while the pipeline output is still normalized to 16 kHz.
    use crate::media::processor::ProcessorChain;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut chain = ProcessorChain::new(16000);
    chain.raw_tap = Some(tx);

    let mut frame = AudioFrame {
        track_id: "caller".to_string(),
        samples: Samples::RTP {
            sequence_number: 1,
            payload_type: 0,            // PCMU
            payload: vec![0xFFu8; 160], // 20ms of silence at 8 kHz
        },
        timestamp: 0,
        sample_rate: 8000,
        channels: 1,
        ..Default::default()
    };

    chain.process_frame(&mut frame)?;

    let tapped = rx.recv().await.unwrap();
    assert_eq!(tapped.sample_rate, 8000, "tap must keep the native rate");
    assert!(
        tapped.src_packet.is_none(),
        "tap should not carry RTP payload"
    );
    match tapped.samples {
        Samples::PCM { samples } => assert_eq!(samples.len(), 160),
        _ => panic!("tapped frame should contain PCM"),
    }

    // The pipeline frame itself must still be normalized to 16 kHz mono.
    assert_eq!(frame.sample_rate, 16000);
    assert_eq!(frame.channels, 1);
    match frame.samples {
        Samples::PCM { samples } => assert!(
            (300..=340).contains(&samples.len()),
            "unexpected resampled length: {}",
            samples.len()
        ),
        _ => panic!("pipeline frame should contain PCM"),
    }
    Ok(())
}

/// Byte-level WAV header validation: record exactly 1s of 8 kHz audio per leg
/// in native mode and verify the RIFF/fmt/data chunks describe
/// "PCM, 8000 Hz, 2 channels, 16-bit" with consistent sizes.
#[tokio::test]
async fn test_recorder_native_wav_header_bytes() -> Result<()> {
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_native_header.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption {
        native_samplerate: Some(true),
        ptime: 200,
        ..Default::default()
    };

    let recorder = Arc::new(Recorder::new(
        cancel_token.clone(),
        "caller".to_string(),
        config,
    ));
    let (tx, rx) = mpsc::unbounded_channel();

    let recorder_clone = recorder.clone();
    let recorder_handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    // 50 frames x 160 samples (20ms @ 8kHz) = exactly 1s per leg.
    for i in 0..50 {
        let caller_samples: PcmBuf = (0..160)
            .map(|j| {
                let t = (i * 160 + j) as f32 / 8000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 8000.0) as Sample
            })
            .collect();
        let callee_samples: PcmBuf = (0..160)
            .map(|j| {
                let t = (i * 160 + j) as f32 / 8000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 8000.0) as Sample
            })
            .collect();
        tx.send(AudioFrame {
            track_id: "caller".to_string(),
            samples: Samples::PCM {
                samples: caller_samples,
            },
            timestamp: (i * 20) as u64,
            sample_rate: 8000,
            channels: 1,
            ..Default::default()
        })?;
        tx.send(AudioFrame {
            track_id: "callee".to_string(),
            samples: Samples::PCM {
                samples: callee_samples,
            },
            timestamp: (i * 20) as u64,
            sample_rate: 8000,
            channels: 1,
            ..Default::default()
        })?;
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }

    // Let the final interval tick drain, then stop (flush + header rewrite).
    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    recorder.stop_recording()?;
    recorder_handle.await??;

    let bytes = std::fs::read(&file_path)?;
    assert!(bytes.len() >= 44, "file too short for a WAV header");

    let dump: String = bytes[..44].iter().map(|b| format!("{:02x}", b)).collect();
    println!("wav header (44 bytes): {}", dump);

    let u16_at = |off: usize| u16::from_le_bytes(bytes[off..off + 2].try_into().unwrap());
    let u32_at = |off: usize| u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());

    // RIFF chunk descriptor
    assert_eq!(&bytes[0..4], b"RIFF");
    let riff_size = u32_at(4);
    assert_eq!(&bytes[8..12], b"WAVE");

    // fmt chunk: PCM, 2ch, 8kHz, 16-bit
    assert_eq!(&bytes[12..16], b"fmt ");
    assert_eq!(u32_at(16), 16, "fmt chunk size");
    let format_tag = u16_at(20);
    let channels = u16_at(22);
    let sample_rate = u32_at(24);
    let byte_rate = u32_at(28);
    let block_align = u16_at(32);
    let bits_per_sample = u16_at(34);
    assert_eq!(format_tag, 0x0001, "must be uncompressed PCM");
    assert_eq!(channels, 2, "caller+callee interleaved stereo");
    assert_eq!(sample_rate, 8000, "native rate from the caller leg");
    assert_eq!(byte_rate, 8000 * 2 * 2, "sample_rate * block_align");
    assert_eq!(block_align, 4, "channels * bits/8");
    assert_eq!(bits_per_sample, 16);

    // data chunk + size consistency
    assert_eq!(&bytes[36..40], b"data");
    let data_size = u32_at(40) as usize;
    assert_eq!(
        riff_size as usize,
        data_size + 36,
        "RIFF size must match data size"
    );
    assert_eq!(
        bytes.len(),
        data_size + 44,
        "file length must match RIFF header sizes"
    );
    // ~1s of stereo 16-bit @8kHz = 32000 bytes; tolerate scheduler padding.
    assert!(
        (32000..=64000).contains(&data_size),
        "unexpected data size: {}",
        data_size
    );
    assert_eq!(
        data_size % 4,
        0,
        "data must be frame-aligned (2ch x 16-bit)"
    );

    Ok(())
}
