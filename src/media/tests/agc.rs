use audio_codec::samples_to_bytes;

use crate::media::{
    AudioFrame, Samples, agc::{AGCOption, AutomaticGainControl}, processor::Processor,
};
use std::{fs::File, io::Write};

#[test]
fn test_basic_processing() {
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/hello_book_course_zh_16k.wav").unwrap();
    let mut agc = AutomaticGainControl::new(sample_rate, AGCOption::default()).unwrap();
    let mut out_file = File::create("fixtures/hello_book_course_zh_16k_agc.pcm.decoded").unwrap();
    let frame_size = (sample_rate as usize) / 50; // 20ms
    for chunk in all_samples.chunks(frame_size) {
        let mut frame = AudioFrame {
            samples: Samples::PCM {
                samples: chunk.to_vec(),
            },
            sample_rate,
            track_id: "test".to_string(),
            timestamp: 0,
            channels: 1,
            src_packet: None,
            ..Default::default()
        };
        agc.process_frame(&mut frame).unwrap();
        let samples = match frame.samples {
            Samples::PCM { samples } => samples,
            _ => panic!("Expected PCM samples"),
        };
        out_file.write_all(&samples_to_bytes(&samples)).unwrap();
    }
    println!(
        "ffplay -f s16le -ar {} fixtures/hello_book_course_zh_16k_agc.pcm.decoded",
        sample_rate
    );
}

#[test]
fn test_silence_does_not_pump_gain() {
    // Feed quiet speech to settle gain above 1.0, then feed pure silence
    // followed by low-level noise — gain must not be driven up during silence.
    let sample_rate = 16000u32;
    let frame_size = 320; // 20ms

    let mut agc = AutomaticGainControl::new(sample_rate, AGCOption::default()).unwrap();

    // ~2s of quiet 200Hz sine at amplitude 3000 (~-20 dBFS peak)
    let amplitude = 3000.0;
    let freq = 200.0;
    let speech_frames = 100;
    for i in 0..speech_frames {
        let mut samples = Vec::with_capacity(frame_size);
        for n in 0..frame_size {
            let t = ((i * frame_size + n) as f32) / sample_rate as f32;
            let v = (amplitude * (2.0 * std::f32::consts::PI * freq * t).sin()) as i16;
            samples.push(v);
        }
        let mut frame = make_frame(samples, sample_rate);
        agc.process_frame(&mut frame).unwrap();
    }

    // Snapshot gain after speech
    let gain_after_speech = agc.current_gain_for_test();

    // ~3s of pure silence — must not pump gain up
    for _ in 0..150 {
        let mut frame = make_frame(vec![0i16; frame_size], sample_rate);
        agc.process_frame(&mut frame).unwrap();
    }
    let gain_after_silence = agc.current_gain_for_test();

    // ~3s of low-level noise (-60 dBFS, below silence threshold) — must also not pump
    let noise_amp = (i16::MAX as f32 * 10f32.powf(-60.0 / 20.0)) as i16;
    for i in 0..150 {
        let mut samples = Vec::with_capacity(frame_size);
        for n in 0..frame_size {
            // alternating ± to keep peak ~= noise_amp
            let s = if (i + n) % 2 == 0 { noise_amp } else { -noise_amp };
            samples.push(s);
        }
        let mut frame = make_frame(samples, sample_rate);
        agc.process_frame(&mut frame).unwrap();
    }
    let gain_after_noise = agc.current_gain_for_test();

    println!(
        "gains: speech={:.3} silence={:.3} noise={:.3}",
        gain_after_speech, gain_after_silence, gain_after_noise
    );

    // Gain must not climb during silent stretches (allow tiny smoothing drift).
    assert!(
        gain_after_silence <= gain_after_speech + 0.01,
        "gain drifted up during silence: {} -> {}",
        gain_after_speech,
        gain_after_silence
    );
    assert!(
        gain_after_noise <= gain_after_speech + 0.01,
        "gain drifted up during sub-threshold noise: {} -> {}",
        gain_after_speech,
        gain_after_noise
    );
}

#[test]
fn test_agc_performance() {
    if cfg!(debug_assertions) {
        println!("Skipping AGC performance test in debug mode.");
        return;
    }
    use std::time::Instant;
    let sample_rate = 16000u32;
    let frame_size = 320; // 20ms at 16kHz
    let mut agc = AutomaticGainControl::new(sample_rate, AGCOption::default()).unwrap();

    let mut samples = Vec::with_capacity(frame_size);
    for i in 0..frame_size {
        samples.push((i % 100) as i16);
    }

    let iterations = 5000;
    let start = Instant::now();

    for _ in 0..iterations {
        let mut frame = make_frame(samples.clone(), sample_rate);
        agc.process_frame(&mut frame).unwrap();
    }

    let duration = start.elapsed();
    let avg_time = duration / iterations as u32;

    println!("Total time for {} iterations: {:?}", iterations, duration);
    println!("Average time per frame: {:?}", avg_time);
    println!("FPS: {:.2}", 1.0 / avg_time.as_secs_f64());
}

fn make_frame(samples: Vec<i16>, sample_rate: u32) -> AudioFrame {
    AudioFrame {
        samples: Samples::PCM { samples },
        sample_rate,
        track_id: "test".to_string(),
        timestamp: 0,
        channels: 1,
        ..Default::default()
    }
}
