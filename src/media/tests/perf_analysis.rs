#[cfg(test)]
mod tests {
    use crate::media::denoiser::NoiseReducer;
    use crate::media::processor::Processor;
    use crate::media::vad::VADOption;
    use crate::media::vad::tiny_silero::TinySilero;
    use crate::media::{AudioFrame, Samples};
    use audio_codec::g722::G722Encoder;
    use audio_codec::g729::{G729Decoder, G729Encoder};
    use audio_codec::opus::OpusEncoder;
    use audio_codec::pcmu::{PcmuDecoder, PcmuEncoder};
    use audio_codec::{Decoder, Encoder, Resampler};
    use rand::Rng;
    use std::time::{Duration, Instant};

    #[test]
    fn test_pipeline_performance() {
        // ignore when not release mode
        if cfg!(debug_assertions) {
            println!("Skipping pipeline performance test in debug mode.");
            return;
        }
        let duration_sec = 60;
        let chunk_ms = 20;
        let sample_rate_pcmu = 8000;
        let sample_rate_g729 = 8000;

        let samples_per_chunk_pcmu = (sample_rate_pcmu * chunk_ms) / 1000; // 160
        let _samples_per_chunk_g729 = (sample_rate_g729 * chunk_ms) / 1000; // 160
        // G.729 is 10 bytes per 10ms frame. So 20ms = 2 frames = 20 bytes.
        let bytes_per_chunk_g729 = 20;

        let total_chunks = (duration_sec * 1000) / chunk_ms;

        let mut rng = rand::rng();

        // Data for PCMU scenario
        let input_data_pcmu: Vec<u8> = (0..total_chunks * samples_per_chunk_pcmu)
            .map(|_| rng.random())
            .collect();

        // Data for G729 scenario
        let input_data_g729: Vec<u8> = (0..total_chunks * bytes_per_chunk_g729)
            .map(|_| rng.random())
            .collect();

        // --- Common Components ---
        let mut resampler_8k_to_16k = Resampler::new(8000, 16000);
        let mut noise_reducer = NoiseReducer::new(16000);
        let vad_config = VADOption {
            samplerate: 16000,
            ..Default::default()
        };
        let mut vad = TinySilero::new(vad_config).unwrap();
        let mut resampler_16k_to_8k = Resampler::new(16000, 8000);

        // --- Scenario A/B Components ---
        let mut pcmu_decoder = PcmuDecoder::new();
        let mut opus_encoder = OpusEncoder::new(16000, 1);
        let mut g722_encoder = G722Encoder::new();
        let mut pcmu_encoder = PcmuEncoder::new();

        // --- Scenario C Components ---
        let mut g729_decoder = G729Decoder::new();
        let mut g729_encoder = G729Encoder::new();

        // --- Timers ---
        let mut t_decode_resample = Duration::ZERO;
        let mut t_denoise = Duration::ZERO;
        let mut t_vad = Duration::ZERO;
        let mut t_opus = Duration::ZERO;
        let mut t_g722 = Duration::ZERO;
        let mut t_encode_pcmu = Duration::ZERO;

        // Scenario C Timers
        let mut t_g729_decode_resample = Duration::ZERO;
        let mut t_g729_denoise = Duration::ZERO;
        let mut t_g729_vad = Duration::ZERO;
        let mut t_g729_encode_g722 = Duration::ZERO;
        let mut t_g729_resample_encode_g729 = Duration::ZERO;

        // Scenario D (No Denoise) Timers
        let mut t_nd_decode_resample = Duration::ZERO;
        let mut t_nd_vad = Duration::ZERO;
        let mut t_nd_g722 = Duration::ZERO;
        let mut t_nd_encode_pcmu = Duration::ZERO;

        // Scenario E (No Denoise) Timers
        let mut t_nd_g729_decode_resample = Duration::ZERO;
        let mut t_nd_g729_vad = Duration::ZERO;
        let mut t_nd_g729_encode_g722 = Duration::ZERO;
        let mut t_nd_g729_resample_encode_g729 = Duration::ZERO;

        // Scenario F (Denoise, No VAD) Timers
        let mut t_nv_decode_resample = Duration::ZERO;
        let mut t_nv_denoise = Duration::ZERO;
        let mut t_nv_g722 = Duration::ZERO;
        let mut t_nv_encode_pcmu = Duration::ZERO;

        // Scenario G (No Denoise, No VAD) Timers
        let mut t_ndnv_decode_resample = Duration::ZERO;
        let mut t_ndnv_g722 = Duration::ZERO;
        let mut t_ndnv_encode_pcmu = Duration::ZERO;

        let mut vad_buffer: Vec<f32> = Vec::with_capacity(1024);
        let vad_chunk_size = 512;

        println!(
            "Starting performance test with {} seconds of audio...",
            duration_sec
        );

        // ==========================================
        // Run Scenario A & B (PCMU Input)
        // ==========================================
        let chunks_pcmu: Vec<&[u8]> = input_data_pcmu
            .chunks(samples_per_chunk_pcmu as usize)
            .collect();

        for chunk in &chunks_pcmu {
            // 1. PCMU -> 16k PCM
            let t0 = Instant::now();
            let pcm_8k = pcmu_decoder.decode(chunk);
            let pcm_16k = resampler_8k_to_16k.resample(&pcm_8k);
            t_decode_resample += t0.elapsed();

            // 2. Denoise
            let t1 = Instant::now();
            let mut frame = AudioFrame {
                samples: Samples::PCM {
                    samples: pcm_16k.clone(),
                },
                ..Default::default()
            };
            noise_reducer.process_frame(&mut frame).unwrap();
            let pcm_16k_denoised = match frame.samples {
                Samples::PCM { samples } => samples,
                _ => panic!("Unexpected sample type"),
            };
            t_denoise += t1.elapsed();

            // 3. VAD
            let t2 = Instant::now();
            let samples_f32: Vec<f32> = pcm_16k_denoised
                .iter()
                .map(|&s| s as f32 / 32768.0)
                .collect();
            vad_buffer.extend_from_slice(&samples_f32);

            while vad_buffer.len() >= vad_chunk_size {
                let vad_chunk = &vad_buffer[0..vad_chunk_size];
                vad.predict(vad_chunk);
                vad_buffer.drain(0..vad_chunk_size);
            }
            t_vad += t2.elapsed();

            // 4a. Encode Opus
            let t3 = Instant::now();
            opus_encoder.encode(&pcm_16k_denoised);
            t_opus += t3.elapsed();

            // 4b. Encode G722
            let t3b = Instant::now();
            g722_encoder.encode(&pcm_16k_denoised);
            t_g722 += t3b.elapsed();

            // 5. Encode PCMU (from 16k)
            let t4 = Instant::now();
            let pcm_8k_out = resampler_16k_to_8k.resample(&pcm_16k_denoised);
            pcmu_encoder.encode(&pcm_8k_out);
            t_encode_pcmu += t4.elapsed();
        }

        vad_buffer.clear();

        // ==========================================
        // Run Scenario C (G729 Input)
        // ==========================================
        let chunks_g729: Vec<&[u8]> = input_data_g729
            .chunks(bytes_per_chunk_g729 as usize)
            .collect();

        for chunk in &chunks_g729 {
            // 1. G729 -> 16k PCM
            let t0 = Instant::now();
            let pcm_8k = g729_decoder.decode(chunk);
            let pcm_16k = resampler_8k_to_16k.resample(&pcm_8k);
            t_g729_decode_resample += t0.elapsed();

            // 2. Denoise
            let t1 = Instant::now();
            let mut frame = AudioFrame {
                samples: Samples::PCM {
                    samples: pcm_16k.clone(),
                },
                ..Default::default()
            };
            noise_reducer.process_frame(&mut frame).unwrap();
            let pcm_16k_denoised = match frame.samples {
                Samples::PCM { samples } => samples,
                _ => panic!("Unexpected sample type"),
            };
            t_g729_denoise += t1.elapsed();

            // 3. VAD
            let t2 = Instant::now();
            let samples_f32: Vec<f32> = pcm_16k_denoised
                .iter()
                .map(|&s| s as f32 / 32768.0)
                .collect();
            vad_buffer.extend_from_slice(&samples_f32);

            while vad_buffer.len() >= vad_chunk_size {
                let vad_chunk = &vad_buffer[0..vad_chunk_size];
                vad.predict(vad_chunk);
                vad_buffer.drain(0..vad_chunk_size);
            }
            t_g729_vad += t2.elapsed();

            // 4. Encode G722
            let t3 = Instant::now();
            g722_encoder.encode(&pcm_16k_denoised);
            t_g729_encode_g722 += t3.elapsed();

            // 5. Resample (16k->8k) + G729 Encode
            let t4 = Instant::now();
            let pcm_8k_out = resampler_16k_to_8k.resample(&pcm_16k_denoised);
            g729_encoder.encode(&pcm_8k_out);
            t_g729_resample_encode_g729 += t4.elapsed();
        }

        vad_buffer.clear();

        // ==========================================
        // Run Scenario D (PCMU Input, No Denoise)
        // ==========================================
        for chunk in &chunks_pcmu {
            // 1. PCMU -> 16k PCM
            let t0 = Instant::now();
            let pcm_8k = pcmu_decoder.decode(chunk);
            let pcm_16k = resampler_8k_to_16k.resample(&pcm_8k);
            t_nd_decode_resample += t0.elapsed();

            // 2. VAD (on raw 16k)
            let t2 = Instant::now();
            let samples_f32: Vec<f32> = pcm_16k.iter().map(|&s| s as f32 / 32768.0).collect();
            vad_buffer.extend_from_slice(&samples_f32);

            while vad_buffer.len() >= vad_chunk_size {
                let vad_chunk = &vad_buffer[0..vad_chunk_size];
                vad.predict(vad_chunk);
                vad_buffer.drain(0..vad_chunk_size);
            }
            t_nd_vad += t2.elapsed();

            // 3. Encode G722 (on raw 16k)
            let t3 = Instant::now();
            g722_encoder.encode(&pcm_16k);
            t_nd_g722 += t3.elapsed();

            // 4. Encode PCMU (from 16k)
            let t4 = Instant::now();
            let pcm_8k_out = resampler_16k_to_8k.resample(&pcm_16k);
            pcmu_encoder.encode(&pcm_8k_out);
            t_nd_encode_pcmu += t4.elapsed();
        }

        vad_buffer.clear();

        // ==========================================
        // Run Scenario E (G729 Input, No Denoise)
        // ==========================================
        for chunk in &chunks_g729 {
            // 1. G729 -> 16k PCM
            let t0 = Instant::now();
            let pcm_8k = g729_decoder.decode(chunk);
            let pcm_16k = resampler_8k_to_16k.resample(&pcm_8k);
            t_nd_g729_decode_resample += t0.elapsed();

            // 2. VAD (on raw 16k)
            let t2 = Instant::now();
            let samples_f32: Vec<f32> = pcm_16k.iter().map(|&s| s as f32 / 32768.0).collect();
            vad_buffer.extend_from_slice(&samples_f32);

            while vad_buffer.len() >= vad_chunk_size {
                let vad_chunk = &vad_buffer[0..vad_chunk_size];
                vad.predict(vad_chunk);
                vad_buffer.drain(0..vad_chunk_size);
            }
            t_nd_g729_vad += t2.elapsed();

            // 3. Encode G722 (on raw 16k)
            let t3 = Instant::now();
            g722_encoder.encode(&pcm_16k);
            t_nd_g729_encode_g722 += t3.elapsed();

            // 4. Resample (16k->8k) + G729 Encode
            let t4 = Instant::now();
            let pcm_8k_out = resampler_16k_to_8k.resample(&pcm_16k);
            g729_encoder.encode(&pcm_8k_out);
            t_nd_g729_resample_encode_g729 += t4.elapsed();
        }

        // ==========================================
        // Run Scenario F (PCMU Input, Denoise, No VAD)
        // ==========================================
        for chunk in &chunks_pcmu {
            // 1. PCMU -> 16k PCM
            let t0 = Instant::now();
            let pcm_8k = pcmu_decoder.decode(chunk);
            let pcm_16k = resampler_8k_to_16k.resample(&pcm_8k);
            t_nv_decode_resample += t0.elapsed();

            // 2. Denoise
            let t1 = Instant::now();
            let mut frame = AudioFrame {
                samples: Samples::PCM {
                    samples: pcm_16k.clone(),
                },
                ..Default::default()
            };
            noise_reducer.process_frame(&mut frame).unwrap();
            let pcm_16k_denoised = match frame.samples {
                Samples::PCM { samples } => samples,
                _ => panic!("Unexpected sample type"),
            };
            t_nv_denoise += t1.elapsed();

            // 3. Encode G722 (on denoised 16k)
            let t3 = Instant::now();
            g722_encoder.encode(&pcm_16k_denoised);
            t_nv_g722 += t3.elapsed();

            // 4. Encode PCMU (from 16k)
            let t4 = Instant::now();
            let pcm_8k_out = resampler_16k_to_8k.resample(&pcm_16k_denoised);
            pcmu_encoder.encode(&pcm_8k_out);
            t_nv_encode_pcmu += t4.elapsed();
        }

        // ==========================================
        // Run Scenario G (PCMU Input, No Denoise, No VAD)
        // ==========================================
        for chunk in &chunks_pcmu {
            // 1. PCMU -> 16k PCM
            let t0 = Instant::now();
            let pcm_8k = pcmu_decoder.decode(chunk);
            let pcm_16k = resampler_8k_to_16k.resample(&pcm_8k);
            t_ndnv_decode_resample += t0.elapsed();

            // 2. Encode G722 (on raw 16k)
            let t3 = Instant::now();
            g722_encoder.encode(&pcm_16k);
            t_ndnv_g722 += t3.elapsed();

            // 3. Encode PCMU (from 16k)
            let t4 = Instant::now();
            let pcm_8k_out = resampler_16k_to_8k.resample(&pcm_16k);
            pcmu_encoder.encode(&pcm_8k_out);
            t_ndnv_encode_pcmu += t4.elapsed();
        }

        // Calculate totals for Opus Scenario
        let total_time_opus = t_decode_resample + t_denoise + t_vad + t_opus + t_encode_pcmu;
        let total_micros_opus = total_time_opus.as_micros() as f64;

        // Calculate totals for G722 Scenario
        let total_time_g722 = t_decode_resample + t_denoise + t_vad + t_g722 + t_encode_pcmu;
        let total_micros_g722 = total_time_g722.as_micros() as f64;

        // Calculate totals for G729 Scenario
        let total_time_g729 = t_g729_decode_resample
            + t_g729_denoise
            + t_g729_vad
            + t_g729_encode_g722
            + t_g729_resample_encode_g729;
        let total_micros_g729 = total_time_g729.as_micros() as f64;

        // Calculate totals for Scenario D (No Denoise)
        let total_time_nd = t_nd_decode_resample + t_nd_vad + t_nd_g722 + t_nd_encode_pcmu;
        let total_micros_nd = total_time_nd.as_micros() as f64;

        // Calculate totals for Scenario E (No Denoise)
        let total_time_nd_g729 = t_nd_g729_decode_resample
            + t_nd_g729_vad
            + t_nd_g729_encode_g722
            + t_nd_g729_resample_encode_g729;
        let total_micros_nd_g729 = total_time_nd_g729.as_micros() as f64;

        // Calculate totals for Scenario F (Denoise, No VAD)
        let total_time_nv = t_nv_decode_resample + t_nv_denoise + t_nv_g722 + t_nv_encode_pcmu;
        let total_micros_nv = total_time_nv.as_micros() as f64;

        // Calculate totals for Scenario G (No Denoise, No VAD)
        let total_time_ndnv = t_ndnv_decode_resample + t_ndnv_g722 + t_ndnv_encode_pcmu;
        let total_micros_ndnv = total_time_ndnv.as_micros() as f64;

        println!("\nPerformance Analysis Results ({}s audio):", duration_sec);

        println!("\n--- Scenario A: PCMU In -> Opus Out (PCMU Out) [Denoise + VAD] ---");
        println!(
            "Total Processing Time: {:.2} ms",
            total_time_opus.as_millis()
        );
        println!(
            "Real-time Factor: {:.4}",
            total_time_opus.as_secs_f64() / duration_sec as f64
        );
        println!("Breakdown:");
        println!(
            "1. PCMU Decode + Resample (8k->16k): {:.2} ms ({:.2}%)",
            t_decode_resample.as_millis(),
            (t_decode_resample.as_micros() as f64 / total_micros_opus) * 100.0
        );
        println!(
            "2. Denoise (16k): {:.2} ms ({:.2}%)",
            t_denoise.as_millis(),
            (t_denoise.as_micros() as f64 / total_micros_opus) * 100.0
        );
        println!(
            "3. VAD (16k): {:.2} ms ({:.2}%)",
            t_vad.as_millis(),
            (t_vad.as_micros() as f64 / total_micros_opus) * 100.0
        );
        println!(
            "4. Opus Encode (16k): {:.2} ms ({:.2}%)",
            t_opus.as_millis(),
            (t_opus.as_micros() as f64 / total_micros_opus) * 100.0
        );
        println!(
            "5. Resample (16k->8k) + PCMU Encode: {:.2} ms ({:.2}%)",
            t_encode_pcmu.as_millis(),
            (t_encode_pcmu.as_micros() as f64 / total_micros_opus) * 100.0
        );

        println!("\n--- Scenario B: PCMU In -> G722 Out (PCMU Out) [Denoise + VAD] ---");
        println!(
            "Total Processing Time: {:.2} ms",
            total_time_g722.as_millis()
        );
        println!(
            "Real-time Factor: {:.4}",
            total_time_g722.as_secs_f64() / duration_sec as f64
        );
        println!("Breakdown:");
        println!(
            "1. PCMU Decode + Resample (8k->16k): {:.2} ms ({:.2}%)",
            t_decode_resample.as_millis(),
            (t_decode_resample.as_micros() as f64 / total_micros_g722) * 100.0
        );
        println!(
            "2. Denoise (16k): {:.2} ms ({:.2}%)",
            t_denoise.as_millis(),
            (t_denoise.as_micros() as f64 / total_micros_g722) * 100.0
        );
        println!(
            "3. VAD (16k): {:.2} ms ({:.2}%)",
            t_vad.as_millis(),
            (t_vad.as_micros() as f64 / total_micros_g722) * 100.0
        );
        println!(
            "4. G722 Encode (16k): {:.2} ms ({:.2}%)",
            t_g722.as_millis(),
            (t_g722.as_micros() as f64 / total_micros_g722) * 100.0
        );
        println!(
            "5. Resample (16k->8k) + PCMU Encode: {:.2} ms ({:.2}%)",
            t_encode_pcmu.as_millis(),
            (t_encode_pcmu.as_micros() as f64 / total_micros_g722) * 100.0
        );

        println!("\n--- Scenario C: G729 In -> G722 Out (G729 Out) [Denoise + VAD] ---");
        println!(
            "Total Processing Time: {:.2} ms",
            total_time_g729.as_millis()
        );
        println!(
            "Real-time Factor: {:.4}",
            total_time_g729.as_secs_f64() / duration_sec as f64
        );
        println!("Breakdown:");
        println!(
            "1. G729 Decode + Resample (8k->16k): {:.2} ms ({:.2}%)",
            t_g729_decode_resample.as_millis(),
            (t_g729_decode_resample.as_micros() as f64 / total_micros_g729) * 100.0
        );
        println!(
            "2. Denoise (16k): {:.2} ms ({:.2}%)",
            t_g729_denoise.as_millis(),
            (t_g729_denoise.as_micros() as f64 / total_micros_g729) * 100.0
        );
        println!(
            "3. VAD (16k): {:.2} ms ({:.2}%)",
            t_g729_vad.as_millis(),
            (t_g729_vad.as_micros() as f64 / total_micros_g729) * 100.0
        );
        println!(
            "4. G722 Encode (16k): {:.2} ms ({:.2}%)",
            t_g729_encode_g722.as_millis(),
            (t_g729_encode_g722.as_micros() as f64 / total_micros_g729) * 100.0
        );
        println!(
            "5. Resample (16k->8k) + G729 Encode: {:.2} ms ({:.2}%)",
            t_g729_resample_encode_g729.as_millis(),
            (t_g729_resample_encode_g729.as_micros() as f64 / total_micros_g729) * 100.0
        );

        println!("\n--- Scenario D: PCMU In -> G722 Out (PCMU Out) [NO DENOISE, VAD] ---");
        println!("Total Processing Time: {:.2} ms", total_time_nd.as_millis());
        println!(
            "Real-time Factor: {:.4}",
            total_time_nd.as_secs_f64() / duration_sec as f64
        );
        println!("Breakdown:");
        println!(
            "1. PCMU Decode + Resample (8k->16k): {:.2} ms ({:.2}%)",
            t_nd_decode_resample.as_millis(),
            (t_nd_decode_resample.as_micros() as f64 / total_micros_nd) * 100.0
        );
        println!(
            "2. VAD (16k): {:.2} ms ({:.2}%)",
            t_nd_vad.as_millis(),
            (t_nd_vad.as_micros() as f64 / total_micros_nd) * 100.0
        );
        println!(
            "3. G722 Encode (16k): {:.2} ms ({:.2}%)",
            t_nd_g722.as_millis(),
            (t_nd_g722.as_micros() as f64 / total_micros_nd) * 100.0
        );
        println!(
            "4. Resample (16k->8k) + PCMU Encode: {:.2} ms ({:.2}%)",
            t_nd_encode_pcmu.as_millis(),
            (t_nd_encode_pcmu.as_micros() as f64 / total_micros_nd) * 100.0
        );

        println!("\n--- Scenario E: G729 In -> G722 Out (G729 Out) [NO DENOISE, VAD] ---");
        println!(
            "Total Processing Time: {:.2} ms",
            total_time_nd_g729.as_millis()
        );
        println!(
            "Real-time Factor: {:.4}",
            total_time_nd_g729.as_secs_f64() / duration_sec as f64
        );
        println!("Breakdown:");
        println!(
            "1. G729 Decode + Resample (8k->16k): {:.2} ms ({:.2}%)",
            t_nd_g729_decode_resample.as_millis(),
            (t_nd_g729_decode_resample.as_micros() as f64 / total_micros_nd_g729) * 100.0
        );
        println!(
            "2. VAD (16k): {:.2} ms ({:.2}%)",
            t_nd_g729_vad.as_millis(),
            (t_nd_g729_vad.as_micros() as f64 / total_micros_nd_g729) * 100.0
        );
        println!(
            "3. G722 Encode (16k): {:.2} ms ({:.2}%)",
            t_nd_g729_encode_g722.as_millis(),
            (t_nd_g729_encode_g722.as_micros() as f64 / total_micros_nd_g729) * 100.0
        );
        println!(
            "4. Resample (16k->8k) + G729 Encode: {:.2} ms ({:.2}%)",
            t_nd_g729_resample_encode_g729.as_millis(),
            (t_nd_g729_resample_encode_g729.as_micros() as f64 / total_micros_nd_g729) * 100.0
        );

        println!("\n--- Scenario F: PCMU In -> G722 Out (PCMU Out) [Denoise, NO VAD] ---");
        println!("Total Processing Time: {:.2} ms", total_time_nv.as_millis());
        println!(
            "Real-time Factor: {:.4}",
            total_time_nv.as_secs_f64() / duration_sec as f64
        );
        println!("Breakdown:");
        println!(
            "1. PCMU Decode + Resample (8k->16k): {:.2} ms ({:.2}%)",
            t_nv_decode_resample.as_millis(),
            (t_nv_decode_resample.as_micros() as f64 / total_micros_nv) * 100.0
        );
        println!(
            "2. Denoise (16k): {:.2} ms ({:.2}%)",
            t_nv_denoise.as_millis(),
            (t_nv_denoise.as_micros() as f64 / total_micros_nv) * 100.0
        );
        println!(
            "3. G722 Encode (16k): {:.2} ms ({:.2}%)",
            t_nv_g722.as_millis(),
            (t_nv_g722.as_micros() as f64 / total_micros_nv) * 100.0
        );
        println!(
            "4. Resample (16k->8k) + PCMU Encode: {:.2} ms ({:.2}%)",
            t_nv_encode_pcmu.as_millis(),
            (t_nv_encode_pcmu.as_micros() as f64 / total_micros_nv) * 100.0
        );

        println!("\n--- Scenario G: PCMU In -> G722 Out (PCMU Out) [NO Denoise, NO VAD] ---");
        println!(
            "Total Processing Time: {:.2} ms",
            total_time_ndnv.as_millis()
        );
        println!(
            "Real-time Factor: {:.4}",
            total_time_ndnv.as_secs_f64() / duration_sec as f64
        );
        println!("Breakdown:");
        println!(
            "1. PCMU Decode + Resample (8k->16k): {:.2} ms ({:.2}%)",
            t_ndnv_decode_resample.as_millis(),
            (t_ndnv_decode_resample.as_micros() as f64 / total_micros_ndnv) * 100.0
        );
        println!(
            "2. G722 Encode (16k): {:.2} ms ({:.2}%)",
            t_ndnv_g722.as_millis(),
            (t_ndnv_g722.as_micros() as f64 / total_micros_ndnv) * 100.0
        );
        println!(
            "3. Resample (16k->8k) + PCMU Encode: {:.2} ms ({:.2}%)",
            t_ndnv_encode_pcmu.as_millis(),
            (t_ndnv_encode_pcmu.as_micros() as f64 / total_micros_ndnv) * 100.0
        );
    }
}
