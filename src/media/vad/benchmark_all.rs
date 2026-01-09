#[cfg(test)]
mod tests {
    use crate::media::vad::VADOption;
    use crate::media::vad::tiny_silero::TinySilero;
    use crate::media::vad::tiny_ten::TinyTen;
    use std::time::Instant;

    #[test]
    fn benchmark_all_vad_engines() {
        let sample_rate = 16000;
        let duration_sec = 60;
        let total_samples = sample_rate * duration_sec;
        let chunk_size = 512; // 32ms for Silero

        // Generate random audio
        let mut rng = rand::rng();
        use rand::Rng;
        let samples_i16: Vec<i16> = (0..total_samples)
            .map(|_| rng.random_range(-32768..32767))
            .collect();

        let samples_f32: Vec<f32> = samples_i16.iter().map(|&s| s as f32 / 32768.0).collect();

        println!("\n--- VAD Benchmark ({}s audio) ---", duration_sec);

        let config = VADOption {
            samplerate: sample_rate as u32,
            ..Default::default()
        };

        // 3. Tiny Silero (F32)
        let mut silero_f32 = TinySilero::new(config.clone()).expect("Failed to create TinySilero");
        // Check weights
        let start = Instant::now();
        let mut count = 0;
        for chunk in samples_f32.chunks(chunk_size) {
            if chunk.len() == chunk_size {
                silero_f32.predict(chunk);
                count += 1;
            }
        }
        let duration = start.elapsed();
        println!(
            "TinySilero (F32): {:?} ({} frames, {:.2} FPS)",
            duration,
            count,
            count as f64 / duration.as_secs_f64()
        );

        // 6. Tiny Ten (F32)
        let mut ten_tiny = TinyTen::new(config.clone()).expect("Failed to create TinyVad");
        let start = Instant::now();
        let mut count = 0;
        // TinyVad usually consumes chunks of specific size, let's use 512 (32ms) as input
        for chunk in samples_f32.chunks(chunk_size) {
            if chunk.len() == chunk_size {
                ten_tiny.predict(chunk);
                count += 1;
            }
        }
        let duration = start.elapsed();
        println!(
            "TinyTen VAD:       {:?} ({} frames, {:.2} FPS)",
            duration,
            count,
            count as f64 / duration.as_secs_f64()
        );
    }
}
