#[cfg(test)]
mod tests {
    use crate::media::vad::VADOption;
    use crate::media::vad::VadEngine;
    use crate::media::vad::tiny_silero::TinySilero;
    use crate::media::{AudioFrame, Samples};
    use std::time::Instant;

    #[test]
    fn benchmark_all_vad_engines() {
        let sample_rate = 16000;
        let duration_sec = 60;
        let total_samples = sample_rate * duration_sec;
        let chunk_size = 320; // 20ms for common VAD usage

        // Generate random audio
        let mut rng = rand::rng();
        use rand::Rng;
        let samples_i16: Vec<i16> = (0..total_samples)
            .map(|_| rng.random_range(-32768..32767))
            .collect();

        println!("\n--- VAD Benchmark ({}s audio) ---", duration_sec);

        let config = VADOption {
            samplerate: sample_rate as u32,
            ..Default::default()
        };

        // 1. Tiny Silero
        let mut silero = TinySilero::new(config.clone()).expect("Failed to create TinySilero");
        let start = Instant::now();
        let mut count = 0;
        for chunk in samples_i16.chunks(chunk_size) {
            if chunk.len() == chunk_size {
                let mut frame = AudioFrame {
                    track_id: "test".to_string(),
                    samples: Samples::PCM {
                        samples: chunk.to_vec(),
                    },
                    sample_rate: sample_rate as u32,
                    timestamp: (count * 20) as u64,
                };
                silero.process(&mut frame);
                count += 1;
            }
        }
        let duration = start.elapsed();
        let frame_duration_ms = (chunk_size as f64 * 1000.0) / sample_rate as f64;
        let ms_per_frame = duration.as_secs_f64() * 1000.0 / count as f64;
        println!(
            "RTF for TinySilero (F32): {:.4} (processed {} chunks), {:.2}ms per {:.0}ms frame",
            duration.as_secs_f64() / duration_sec as f64,
            count,
            ms_per_frame,
            frame_duration_ms
        );
    }
}
