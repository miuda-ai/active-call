/// Test that ASR continues to receive frames during hold state
/// This ensures that ASR doesn't error out due to stream interruption
use active_call::media::{
    AudioFrame, Samples, processor::Processor, volume_control::HoldProcessor,
};
use anyhow::Result;

#[tokio::test]
async fn test_hold_processor_continues_frame_flow() -> Result<()> {
    // Create a HoldProcessor
    let mut processor = HoldProcessor::new();

    // Set to hold state
    processor.set_hold(true);

    // Create a frame with audio samples
    let original_samples = vec![100, 200, 300, 400, 500];
    let mut frame = AudioFrame {
        track_id: "test-track".to_string(),
        samples: Samples::PCM {
            samples: original_samples.clone(),
        },
        timestamp: 0,
        sample_rate: 16000,
        channels: 1,
        src_packet: None,
        ..Default::default()
    };

    // Process the frame (in hold state)
    processor.process_frame(&mut frame)?;

    // Verify that:
    // 1. Frame structure is maintained (not replaced with Samples::Empty)
    // 2. Samples are all zeroed (silenced)
    // 3. The Vec length is preserved (ASR will still receive the frame)
    match &frame.samples {
        Samples::PCM { samples } => {
            // Frame should still have samples (not empty)
            assert!(
                !samples.is_empty(),
                "Frame should not be empty - ASR needs continuous frames"
            );

            // Length should be preserved
            assert_eq!(
                samples.len(),
                original_samples.len(),
                "Frame length should be preserved"
            );

            // All samples should be zero (silenced)
            for sample in samples {
                assert_eq!(*sample, 0, "All samples should be silenced (zero)");
            }

            println!("✓ Hold processor maintains frame structure with silence");
            println!("  Original length: {}", original_samples.len());
            println!("  Silenced length: {}", samples.len());
            println!("  This ensures ASR continues receiving frames without interruption");
        }
        _ => {
            panic!("Frame samples should remain as PCM type");
        }
    }

    // Now test resume (hold off)
    processor.set_hold(false);

    let mut frame2 = AudioFrame {
        track_id: "test-track".to_string(),
        samples: Samples::PCM {
            samples: vec![100, 200, 300],
        },
        timestamp: 0,
        sample_rate: 16000,
        channels: 1,
        src_packet: None,
        ..Default::default()
    };

    processor.process_frame(&mut frame2)?;

    // Verify samples are not modified when not on hold
    match &frame2.samples {
        Samples::PCM { samples } => {
            assert_eq!(
                samples[0], 100,
                "Samples should not be modified when not on hold"
            );
            assert_eq!(
                samples[1], 200,
                "Samples should not be modified when not on hold"
            );
            assert_eq!(
                samples[2], 300,
                "Samples should not be modified when not on hold"
            );
            println!("✓ After resume, audio passes through unmodified");
        }
        _ => {
            panic!("Frame samples should remain as PCM type");
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_hold_with_empty_frame() -> Result<()> {
    let mut processor = HoldProcessor::new();
    processor.set_hold(true);

    // Test with empty samples
    let mut frame = AudioFrame {
        track_id: "test-track".to_string(),
        samples: Samples::PCM { samples: vec![] },
        timestamp: 0,
        sample_rate: 16000,
        channels: 1,
        src_packet: None,
        ..Default::default()
    };

    // Should not panic with empty samples
    processor.process_frame(&mut frame)?;

    match &frame.samples {
        Samples::PCM { samples } => {
            assert!(samples.is_empty(), "Empty frame should remain empty");
        }
        _ => panic!("Frame type should not change"),
    }

    Ok(())
}

#[tokio::test]
async fn test_hold_with_non_pcm_samples() -> Result<()> {
    let mut processor = HoldProcessor::new();
    processor.set_hold(true);

    // Test with RTP samples (should be ignored)
    let mut frame = AudioFrame {
        track_id: "test-track".to_string(),
        samples: Samples::RTP {
            payload_type: 0,
            payload: vec![1, 2, 3, 4],
            sequence_number: 100,
        },
        timestamp: 0,
        sample_rate: 16000,
        channels: 1,
        src_packet: None,
        ..Default::default()
    };

    processor.process_frame(&mut frame)?;

    // RTP samples should remain unchanged (HoldProcessor only affects PCM)
    match &frame.samples {
        Samples::RTP { payload, .. } => {
            assert_eq!(
                payload,
                &vec![1, 2, 3, 4],
                "RTP payload should not be modified"
            );
        }
        _ => panic!("Frame type should not change"),
    }

    Ok(())
}
