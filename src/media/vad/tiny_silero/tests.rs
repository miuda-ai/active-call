use super::*;

fn new_vad() -> TinySilero {
    TinySilero::new(VADOption {
        samplerate: 16000,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn test_tiny_silero_load_and_run() -> Result<()> {
    let mut vad = new_vad();

    // Create dummy audio
    let audio = vec![0.0; 512];

    // Run a few times
    for i in 0..10 {
        let prob = vad.predict(&audio);
        println!("Step {}: prob = {}", i, prob);
    }

    Ok(())
}

#[test]
fn test_process_buffers_chunks() -> Result<()> {
    let mut vad = new_vad();
    assert!(vad.process_samples(&[0i16; 300]).is_empty());
    assert_eq!(vad.process_samples(&[0i16; 800]).len(), 2);
    assert!(vad.last_probability().is_some());
    Ok(())
}

/// Regression test: fast_tanh panicked with "index out of bounds: the len is 1024
/// but the index is 1024" for x values one ULP below the upper guard (5.0).
///
/// Root cause: `1023.0_f32 / 10.0_f32` rounds UP to 102.30000305..., so for
/// x = 4.9999995 (one ULP below 5.0), the interpolation index `i` becomes exactly
/// 1023, making `table[i + 1] = table[1024]` an out-of-bounds access.
///
/// Fix: clamp i to at most 1022 via `.min(1022)`.
#[test]
fn test_fast_tanh_boundary_no_panic() {
    // One ULP below 5.0 — passes the guard `x >= 5.0` check (returns false),
    // but the old code still computed i = 1023.
    let x: f32 = f32::from_bits(5.0_f32.to_bits() - 1);
    assert!(x < 5.0, "must be strictly below the guard");

    // Verify this is exactly the triggering input that caused the panic in old code:
    //   idx = (x + 5.0) * (1023.0_f32 / 10.0_f32)
    // 1023.0_f32 / 10.0_f32 rounds UP to 102.30000305..., so i becomes 1023 -> OOB.
    let idx = (x + 5.0_f32) * (1023.0_f32 / 10.0_f32);
    assert_eq!(
        idx as usize, 1023,
        "old code: i == 1023, so table[i+1] = table[1024] would panic"
    );

    // With the fix (.min(1022)), fast_tanh must NOT panic and must be near 1.0.
    let result = fast_tanh(x);
    assert!(
        result > 0.999 && result <= 1.0,
        "expected a value very close to 1.0, got {}",
        result
    );
}

/// Regression test: fast_sigmoid panicked with the same out-of-bounds error.
///
/// For x = 7.9999995 (one ULP below 8.0), f32 addition `x + 8.0` rounds up to
/// exactly 16.0, and `16.0 * 63.9375 = 1023.0` exactly, again giving i = 1023.
#[test]
fn test_fast_sigmoid_boundary_no_panic() {
    let x: f32 = f32::from_bits(8.0_f32.to_bits() - 1);
    assert!(x < 8.0, "must be strictly below the guard");

    // f32 addition 7.9999995 + 8.0 rounds to 16.0; 16.0 * 63.9375 == 1023.0,
    // so the old code produced i = 1023 -> OOB.
    let idx = (x + 8.0_f32) * (1023.0_f32 / 16.0_f32);
    assert_eq!(
        idx as usize, 1023,
        "old code: i == 1023, so table[i+1] = table[1024] would panic"
    );

    // With the fix, fast_sigmoid must NOT panic and must be near 1.0.
    let result = fast_sigmoid(x);
    assert!(
        result > 0.999 && result <= 1.0,
        "expected a value very close to 1.0, got {}",
        result
    );
}

fn load_pcm(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

/// 16 kHz mono PCM16 LE speech clips synthesized with Yandex SpeechKit TTS.
macro_rules! audio {
    ($name:literal) => {
        load_pcm(include_bytes!(concat!(
            "../../../../fixtures/vad/",
            $name,
            ".raw"
        )))
    };
}

fn probs(samples: &[i16]) -> Vec<f32> {
    new_vad().process_samples(samples)
}

fn clips() -> Vec<(&'static str, Vec<i16>)> {
    vec![
        ("ru_filipp_greeting", audio!("ru_filipp_greeting")),
        ("ru_alena_address", audio!("ru_alena_address")),
        ("ru_filipp_fast", audio!("ru_filipp_fast")),
        ("ru_alena_slow", audio!("ru_alena_slow")),
    ]
}

fn max(p: &[f32]) -> f32 {
    p.iter().cloned().fold(0.0, f32::max)
}

#[test]
fn test_vad_engine_timestamps_and_decisions() {
    let mut vad = new_vad();
    let pcm = audio!("ru_filipp_greeting");

    let mut results = Vec::new();
    for (i, chunk) in pcm.chunks(320).enumerate() {
        let mut frame = AudioFrame {
            track_id: "test".to_string(),
            samples: Samples::PCM {
                samples: chunk.to_vec(),
            },
            sample_rate: 16000,
            timestamp: 1000 + (i * 20) as u64,
            channels: 1,
            ..Default::default()
        };
        results.extend(vad.process(&mut frame));
    }

    // timestamps start at the first frame and advance by one 512-sample chunk (32 ms)
    assert_eq!(results[0].1, 1000);
    for pair in results.windows(2) {
        assert_eq!(pair[1].1 - pair[0].1, 32);
    }
    // silent lead-in, then speech
    assert!(!results[0].0);
    assert!(results.iter().filter(|(voice, _)| *voice).count() >= 40);
}

/// Plain scalar f32 forward pass over the unquantized weights; the accuracy reference for the
/// i16-quantized SIMD path. Shares the FFT, window and sigmoid/tanh LUTs with production so
/// only the weight quantization (and summation order) differs.
struct F32Reference {
    model: Arc<SileroModel>,
    h: Vec<f32>,
    c: Vec<f32>,
    context: Vec<f32>,
}

impl F32Reference {
    /// With `dequantized`, the "exact" weights are replaced by `q * scale`, which isolates
    /// summation-order effects from quantization error.
    fn new(dequantized: bool) -> Self {
        let mut model = SileroModel::new().unwrap();
        if dequantized {
            let dq = |w: &mut QWeights, oc: usize| {
                for (i, v) in w.exact.iter_mut().enumerate() {
                    *v = w.q[i] as f32 * w.scales[i % oc];
                }
            };
            for conv in [
                &mut model.enc0,
                &mut model.enc1,
                &mut model.enc2,
                &mut model.enc3,
            ] {
                let oc = conv.out_channels;
                dq(&mut conv.weights, oc);
            }
            dq(&mut model.out_layer.weights, 1);
            dq(&mut model.lstm_w_ih, 4 * HIDDEN_SIZE);
            dq(&mut model.lstm_w_hh, 4 * HIDDEN_SIZE);
        }
        Self {
            model: Arc::new(model),
            h: vec![0.0; HIDDEN_SIZE],
            c: vec![0.0; HIDDEN_SIZE],
            context: vec![0.0; CONTEXT_SIZE],
        }
    }

    fn conv(layer: &Conv1dLayer, input: &[f32], input_len: usize) -> Vec<f32> {
        let w = &layer.weights.exact; // [IC, K, OC]
        let oc = layer.out_channels;
        let out_len =
            (input_len + 2 * layer.padding - (layer.kernel_size - 1) - 1) / layer.stride + 1;
        let mut out = vec![0.0f32; out_len * oc];
        for t in 0..out_len {
            out[t * oc..(t + 1) * oc].copy_from_slice(layer.bias.as_ref().unwrap());
            for ic in 0..layer.in_channels {
                for k in 0..layer.kernel_size {
                    let input_t = (t * layer.stride + k) as isize - layer.padding as isize;
                    if input_t < 0 || input_t >= input_len as isize {
                        continue;
                    }
                    let x = input[input_t as usize * layer.in_channels + ic];
                    let row = &w[(ic * layer.kernel_size + k) * oc..][..oc];
                    for o in 0..oc {
                        out[t * oc + o] += x * row[o];
                    }
                }
            }
        }
        if layer.relu {
            out.iter_mut().for_each(|v| *v = v.max(0.0));
        }
        out
    }

    fn predict(&mut self, audio: &[f32]) -> f32 {
        let m = &self.model;

        let mut padded = self.context.clone();
        padded.extend_from_slice(audio);
        let len = padded.len();
        for i in 0..STFT_PADDING {
            padded.push(padded[len - 2 - i]);
        }
        self.context
            .copy_from_slice(&audio[CHUNK_SIZE - CONTEXT_SIZE..]);

        let mut mags = vec![0.0f32; 4 * 129];
        for t in 0..4 {
            let mut input: Vec<f32> = (0..STFT_WINDOW_SIZE)
                .map(|i| padded[t * STFT_STRIDE + i] * m.window[i])
                .collect();
            let mut spectrum = m.fft.make_output_vec();
            m.fft.process(&mut input, &mut spectrum).unwrap();
            for (i, c) in spectrum.iter().enumerate() {
                mags[t * 129 + i] = c.norm();
            }
        }

        let e0 = Self::conv(&m.enc0, &mags, 4);
        let e1 = Self::conv(&m.enc1, &e0, 4);
        let e2 = Self::conv(&m.enc2, &e1, 2);
        let e3 = Self::conv(&m.enc3, &e2, 1);

        let g4 = 4 * HIDDEN_SIZE;
        let mut gates: Vec<f32> = m
            .lstm_b_ih
            .iter()
            .zip(&m.lstm_b_hh)
            .map(|(a, b)| a + b)
            .collect();
        for (j, (&x, &h)) in e3.iter().zip(&self.h).enumerate() {
            for (g, gate) in gates.iter_mut().enumerate() {
                *gate += x * m.lstm_w_ih.exact[j * g4 + g];
                *gate += h * m.lstm_w_hh.exact[j * g4 + g];
            }
        }
        for j in 0..HIDDEN_SIZE {
            let i_gate = fast_sigmoid(gates[j]);
            let f_gate = fast_sigmoid(gates[HIDDEN_SIZE + j]);
            let g_gate = fast_tanh(gates[2 * HIDDEN_SIZE + j]);
            let o_gate = fast_sigmoid(gates[3 * HIDDEN_SIZE + j]);
            self.c[j] = f_gate * self.c[j] + i_gate * g_gate;
            self.h[j] = o_gate * fast_tanh(self.c[j]);
        }

        let mut sum = m.out_layer.bias.as_ref().unwrap()[0];
        for j in 0..HIDDEN_SIZE {
            sum += m.out_layer.weights.exact[j] * self.h[j].max(0.0);
        }
        fast_sigmoid(sum)
    }
}

/// (max, mean) absolute probability difference between the i16 model and the f32 reference,
/// plus the number of chunks whose decision at 0.5 differs, and the chunk count.
fn i16_vs_f32(pcm: &[i16], dequantized: bool) -> (f32, f32, usize, usize) {
    let got = probs(pcm);
    let mut reference = F32Reference::new(dequantized);
    let want: Vec<f32> = pcm
        .as_chunks::<CHUNK_SIZE>()
        .0
        .iter()
        .map(|c| {
            let chunk: Vec<f32> = c.iter().map(|&s| s as f32 / 32768.0).collect();
            reference.predict(&chunk)
        })
        .collect();
    assert_eq!(got.len(), want.len());

    let diffs: Vec<f32> = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).collect();
    let flips = got
        .iter()
        .zip(&want)
        .filter(|(a, b)| (**a > 0.5) != (**b > 0.5))
        .count();
    (
        diffs.iter().cloned().fold(0.0, f32::max),
        diffs.iter().sum::<f32>() / diffs.len() as f32,
        flips,
        diffs.len(),
    )
}

#[test]
fn i16_quantization_matches_f32_reference() {
    for (name, pcm) in clips() {
        let (dq_max, dq_mean, _, _) = i16_vs_f32(&pcm, true);
        println!(
            "{name}: [same weights, scalar order] max_diff={dq_max:.6} mean_diff={dq_mean:.7}"
        );
        let (max_diff, mean_diff, flips, n) = i16_vs_f32(&pcm, false);
        println!(
            "{name}: [vs exact f32]               max_diff={max_diff:.6} mean_diff={mean_diff:.7} flips@0.5={flips}/{n}"
        );
        assert!(
            dq_max < 1e-3,
            "{name}: SIMD path diverges from scalar: {dq_max}"
        );
        assert!(max_diff < 1e-2, "{name}: max diff {max_diff}");
        assert!(mean_diff < 2e-3, "{name}: mean diff {mean_diff}");
        assert_eq!(flips, 0, "{name}");
    }
}

#[test]
fn silence_is_not_voice() {
    let p = probs(&vec![0i16; 16000]);
    assert_eq!(p.len(), 31);
    assert!(max(&p) < 0.1, "max = {}", max(&p));
}

#[test]
fn low_noise_is_not_voice() {
    // deterministic LCG white noise, ~-40 dBFS
    let mut state = 0x1234_5678u32;
    let noise: Vec<i16> = (0..32000)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            ((state >> 16) as i16) / 100
        })
        .collect();
    let p = probs(&noise);
    assert!(max(&p) < 0.5, "max = {}", max(&p));
}

#[test]
fn synthesized_speech_is_detected() {
    for (name, pcm) in clips() {
        let p = probs(&pcm);
        let voiced = p.iter().filter(|&&x| x > 0.5).count();
        assert!(max(&p) > 0.8, "{name}: max = {}", max(&p));
        assert!(
            voiced >= 40,
            "{name}: only {voiced} voiced chunks of {}",
            p.len()
        );
    }
}

#[test]
fn leading_silence_in_speech_is_not_voice() {
    // TTS clips start with ~200 ms of silence (first 5 chunks = 160 ms)
    for (name, pcm) in clips() {
        let p = probs(&pcm);
        assert!(max(&p[..5]) < 0.1, "{name}: head = {:?}", &p[..5]);
    }
}

#[test]
fn speech_then_silence_drops_back() {
    for (name, mut pcm) in clips() {
        pcm.extend(vec![0i16; 32000]);
        let p = probs(&pcm);
        let tail = &p[p.len() - 5..];
        assert!(max(tail) < 0.1, "{name}: tail = {tail:?}");
    }
}

#[test]
fn streaming_in_odd_pieces_matches_one_shot() {
    let pcm = audio!("ru_filipp_greeting");
    let expected = probs(&pcm);

    let mut vad = new_vad();
    let got: Vec<f32> = pcm
        .chunks(333)
        .flat_map(|c| vad.process_samples(c))
        .collect();

    assert_eq!(got.len(), expected.len());
    for (a, b) in got.iter().zip(&expected) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }
}

/// Notes of 300 ms with two harmonics, like hold music: no speech, but enough to
/// lock the model's recurrent state into hearing none afterwards.
fn tones(secs: usize, amp: f32) -> Vec<i16> {
    let mut seed = 7u32;
    let mut f0 = 220.0f32;
    (0..16000 * secs)
        .map(|i| {
            if i % 4800 == 0 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                f0 = 200.0 + (seed >> 20) as f32 % 800.0;
            }
            let w = 2.0 * std::f32::consts::PI * f0 * i as f32 / 16000.0;
            (amp * (w.sin() + 0.5 * (2.0 * w).sin() + 0.25 * (3.0 * w).sin()) / 1.75) as i16
        })
        .collect()
}

fn voiced(p: &[f32]) -> usize {
    p.iter().filter(|&&p| p > 0.5).count()
}

#[test]
fn speech_is_heard_again_after_hold_music() {
    // Without the reset, none of it is heard after the tones, silence or not.
    for (name, pcm) in clips() {
        let fresh = voiced(&probs(&pcm));
        let mut vad = new_vad();
        vad.process_samples(&tones(120, 3000.0));
        vad.process_samples(&vec![0i16; 12 * 16000]);
        let after = voiced(&vad.process_samples(&pcm));
        assert!(
            2 * after >= fresh,
            "{name}: {after} voiced chunks after the tones, {fresh} from a fresh state"
        );
    }
}

#[test]
fn reset_state_clears_the_recurrent_state() {
    let mut vad = new_vad();
    vad.process_samples(&tones(5, 3000.0));
    assert!(vad.session.c.iter().flatten().any(|&x| x != 0.0));
    vad.reset_state();
    assert!(
        vad.session
            .h
            .iter()
            .chain(&vad.session.c)
            .flatten()
            .all(|&x| x == 0.0)
    );
    assert_eq!(vad.session.quiet_chunks, 0);
}
