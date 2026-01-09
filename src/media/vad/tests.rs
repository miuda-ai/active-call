use super::*;
use crate::event::{SessionEvent, create_event_sender};
use crate::media::{Samples, processor::Processor};
use audio_codec::samples_to_bytes;
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};

#[test]
fn test_vadtype_deserialization() {
    // Test deserializing a string
    let json_str = r#""unknown_vad_type""#;
    let vad_type: VadType = serde_json::from_str(json_str).unwrap();
    match vad_type {
        VadType::Other(s) => assert_eq!(s, "unknown_vad_type"),
        _ => panic!("Expected VadType::Other"),
    }
}

#[derive(Default, Debug)]
struct TestResults {
    speech_segments: Vec<(u64, u64)>, // (start_time, duration)
}

#[tokio::test]
async fn test_vad_with_noise_denoise() {
    use crate::media::denoiser::NoiseReducer;
    use std::fs::File;
    use std::io::Write;
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/noise_gating_zh_16k.wav").unwrap();
    assert_eq!(sample_rate, 16000, "Expected 16kHz sample rate");
    assert!(!all_samples.is_empty(), "Expected non-empty audio file");

    println!(
        "Loaded {} samples from WAV file for testing",
        all_samples.len()
    );
    let nr = NoiseReducer::new(sample_rate as usize);
    let (event_sender, mut event_receiver) = broadcast::channel(128);
    let track_id = "test_track".to_string();

    let mut option = VADOption::default();
    option.r#type = VadType::Silero;
    let token = CancellationToken::new();
    let vad = VadProcessor::create(token, event_sender.clone(), option)
        .expect("Failed to create VAD processor");
    let mut total_duration = 0;
    let (frame_size, chunk_duration_ms) = (320, 20);
    let mut out_file = File::create("fixtures/noise_gating_zh_16k_denoised.pcm.decoded").unwrap();
    for (i, chunk) in all_samples.chunks(frame_size).enumerate() {
        let chunk_vec = chunk.to_vec();
        let chunk_vec = if chunk_vec.len() < frame_size {
            let mut padded = chunk_vec;
            padded.resize(frame_size, 0);
            padded
        } else {
            chunk_vec
        };

        let mut frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM { samples: chunk_vec },
            sample_rate,
            timestamp: i as u64 * chunk_duration_ms,
        };
        nr.process_frame(&mut frame).unwrap();
        vad.process_frame(&mut frame).unwrap();
        let samples = match frame.samples {
            Samples::PCM { samples } => samples,
            _ => panic!("Expected PCM samples"),
        };
        out_file.write_all(&samples_to_bytes(&samples)).unwrap();
        total_duration += chunk_duration_ms;
    }
    sleep(Duration::from_millis(50)).await;

    let mut results = TestResults::default();
    while let Ok(event) = event_receiver.try_recv() {
        match event {
            SessionEvent::Speaking { start_time, .. } => {
                println!("  Speaking event at {}ms", start_time);
            }
            SessionEvent::Silence {
                start_time,
                duration,
                ..
            } => {
                if duration > 0 {
                    println!(
                        "  Silence event: start_time={}ms, duration={}ms",
                        start_time, duration
                    );
                    results.speech_segments.push((start_time, duration));
                }
            }
            _ => {}
        }
    }

    println!(
        "detected {} speech segments, total_duration:{}",
        results.speech_segments.len(),
        total_duration
    );
    assert!(results.speech_segments.len() == 2);
}

#[tokio::test]
async fn test_vad_speech_intervals() {
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/1843344-user.wav").unwrap();

    // Define expected speech intervals (start_ms, end_ms) based on physical energy
    let expected_ranges = vec![
        (9300, 9800),   // Segment 1
        (14700, 15200), // Segment 2
        (19200, 19700), // Segment 3
    ];

    println!("Testing VAD detection on specific intervals...");

    for vad_type in [VadType::Silero, VadType::Ten] {
        println!("Testing VAD type: {:?}", vad_type);
        let (event_sender, mut event_receiver) = broadcast::channel(128);
        let mut option = VADOption::default();
        option.r#type = vad_type.clone();
        // Adjust threshold for Ten to match Silero's sensitivity on this file
        option.voice_threshold = if matches!(vad_type, VadType::Ten) {
            0.5
        } else {
            0.5
        };

        option.speech_padding = 100;
        option.silence_padding = 100;

        let token = CancellationToken::new();
        let vad = VadProcessor::create(token, event_sender, option).unwrap();

        let frame_size = 512;
        let mut processed_samples = 0;

        for chunk in all_samples.chunks(frame_size) {
            let mut chunk_vec = chunk.to_vec();
            chunk_vec.resize(frame_size, 0);

            let mut frame = AudioFrame {
                track_id: "test".to_string(),
                samples: Samples::PCM { samples: chunk_vec },
                sample_rate,
                timestamp: (processed_samples * 1000) / sample_rate as u64,
            };
            processed_samples += frame_size as u64;

            vad.process_frame(&mut frame).unwrap();
        }

        sleep(Duration::from_millis(100)).await;

        let mut detected_starts = Vec::new();
        while let Ok(event) = event_receiver.try_recv() {
            if let SessionEvent::Speaking { start_time, .. } = event {
                // println!("{:?} detected speech start at {}ms", vad_type, start_time);
                detected_starts.push(start_time);
            }
        }

        println!("{:?} Detected starts: {:?}", vad_type, detected_starts);

        for (min_start, max_start) in &expected_ranges {
            let found = detected_starts
                .iter()
                .any(|&t| t >= *min_start && t <= *max_start);
            assert!(
                found,
                "WARNING: {:?} did not detect speech in range {}-{}ms",
                vad_type, min_start, max_start
            );
        }
    }
}

#[tokio::test]
async fn test_silence_timeout() {
    let event_sender = create_event_sender();
    let mut rx = event_sender.subscribe();
    // Configure VAD with silence timeout
    let mut option = VADOption::default();
    option.silence_timeout = Some(1000); // 1 second timeout
    option.silence_padding = 100; // 100ms silence padding
    option.speech_padding = 250; // 250ms speech padding
    option.voice_threshold = 0.5;

    let vad = Box::new(NopVad::new().unwrap());
    let processor = VadProcessor::new(vad, event_sender, option).unwrap();

    // Simulate initial speech
    let mut frame = AudioFrame {
        track_id: "test".to_string(),
        timestamp: 0,
        samples: Samples::PCM {
            samples: vec![1; 160], // 10ms of audio at 16kHz
        },
        sample_rate: 16000,
    };

    // First send some strong speech frames
    for i in 0..20 {
        frame.timestamp = i * 10;
        frame.samples = Samples::PCM {
            samples: vec![100; 160], // Use larger values for speech
        };
        processor.process_frame(&mut frame).unwrap();
    }

    // Add some transition frames
    for i in 20..25 {
        frame.timestamp = i * 10;
        frame.samples = Samples::PCM {
            samples: vec![50; 160], // Decreasing speech intensity
        };
        processor.process_frame(&mut frame).unwrap();
    }

    // Now simulate complete silence with multiple time steps to trigger timeout events
    for i in 25..300 {
        frame.timestamp = i * 10;
        frame.samples = Samples::PCM {
            samples: vec![0; 160],
        };
        processor.process_frame(&mut frame).unwrap();
        if i % 100 == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // Collect and verify events
    let mut events = Vec::new();

    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // We should have:
    // 1. One Speaking event at the start
    // 2. One Silence event when speech ends (with samples)
    // 3. Multiple Silence events for timeout (without samples)
    let speaking_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Speaking { .. }))
        .collect();
    assert_eq!(speaking_events.len(), 1, "Should have one Speaking event");

    let silence_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Silence { .. }))
        .collect();
    assert!(
        silence_events.len() >= 2,
        "Should have at least 2 Silence events"
    );

    // Verify first silence event has samples (end of speech)
    if let SessionEvent::Silence { samples, .. } = &silence_events[0] {
        assert!(samples.is_some(), "First silence event should have samples");
    }

    // Verify subsequent silence events don't have samples (timeout)
    for event in silence_events.iter().skip(1) {
        if let SessionEvent::Silence { samples, .. } = event {
            assert!(
                samples.is_none(),
                "Timeout silence events should not have samples"
            );
        }
    }
}
