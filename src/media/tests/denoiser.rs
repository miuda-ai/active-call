use audio_codec::samples_to_bytes;

use crate::media::{AudioFrame, Samples, denoiser::NoiseReducer, processor::Processor};
use std::{fs::File, io::Write};

#[test]
fn test_basic_processing() {
    let mut reducer = NoiseReducer::new(16000);
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/noise_gating_zh_16k.wav").unwrap();
    let mut out_file = File::create("fixtures/noise_gating_zh_16k_denoised.pcm.decoded").unwrap();
    for chunk in all_samples.chunks(320) {
        let mut frame = AudioFrame {
            samples: Samples::PCM {
                samples: chunk.to_vec(),
            },
            sample_rate,
            track_id: "test".to_string(),
            timestamp: 0,
            channels: 1,
        };
        reducer.process_frame(&mut frame).unwrap();
        let samples = match frame.samples {
            Samples::PCM { samples } => samples,
            _ => panic!("Expected PCM samples"),
        };
        out_file.write_all(&samples_to_bytes(&samples)).unwrap();
    }
    println!("ffplay -f s16le -ar 16000 fixtures/noise_gating_zh_16k_denoised.pcm.decoded");
}

#[test]
fn test_denoiser_performance() {
    if cfg!(debug_assertions) {
        println!("Skipping denoiser performance test in debug mode.");
        return;
    }
    use std::time::Instant;
    let sample_rate = 16000;
    let frame_size = 320; // 20ms at 16kHz
    let mut reducer = NoiseReducer::new(sample_rate);

    // Create a dummy frame with noise
    let mut samples = Vec::with_capacity(frame_size);
    for i in 0..frame_size {
        samples.push((i % 100) as i16);
    }

    let iterations = 5000;
    let start = Instant::now();

    for _ in 0..iterations {
        let mut frame = AudioFrame {
            samples: Samples::PCM {
                samples: samples.clone(),
            },
            sample_rate: sample_rate as u32,
            track_id: "test".to_string(),
            timestamp: 0,
            channels: 1,
        };
        reducer.process_frame(&mut frame).unwrap();
    }

    let duration = start.elapsed();
    let avg_time = duration / iterations as u32;

    println!("Total time for {} iterations: {:?}", iterations, duration);
    println!("Average time per frame: {:?}", avg_time);
    println!("FPS: {:.2}", 1.0 / avg_time.as_secs_f64());
}
