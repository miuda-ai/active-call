use super::{VADOption, VadEngine};
use crate::media::{AudioFrame, Samples};
use anyhow::Result;
use once_cell::sync::Lazy;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

// Constants
const CHUNK_SIZE: usize = 512;
const HIDDEN_SIZE: usize = 128;
const STFT_WINDOW_SIZE: usize = 256;
const STFT_STRIDE: usize = 128;
const CONTEXT_SIZE: usize = 64; // Context from previous chunk for STFT continuity
const STFT_PADDING: usize = 64; // Right padding for STFT (ReflectionPad1d)

static SIGMOID_TABLE: Lazy<[f32; 1024]> = Lazy::new(|| {
    let mut table = [0.0; 1024];
    for i in 0..1024 {
        let x = -8.0 + (i as f32) * (16.0 / 1023.0);
        table[i] = 1.0 / (1.0 + (-x).exp());
    }
    table
});

static TANH_TABLE: Lazy<[f32; 1024]> = Lazy::new(|| {
    let mut table = [0.0; 1024];
    for i in 0..1024 {
        let x = -5.0 + (i as f32) * (10.0 / 1023.0);
        table[i] = x.tanh();
    }
    table
});

#[inline(always)]
fn fast_sigmoid(x: f32) -> f32 {
    if x <= -8.0 {
        return 0.0;
    }
    if x >= 8.0 {
        return 1.0;
    }
    let idx = (x + 8.0) * (1023.0 / 16.0);
    let i = (idx as usize).min(1022);
    let frac = idx - i as f32;
    let table = &*SIGMOID_TABLE;
    table[i] * (1.0 - frac) + table[i + 1] * frac
}

#[inline(always)]
fn fast_tanh(x: f32) -> f32 {
    if x <= -5.0 {
        return -1.0;
    }
    if x >= 5.0 {
        return 1.0;
    }
    let idx = (x + 5.0) * (1023.0 / 10.0);
    let i = (idx as usize).min(1022);
    let frac = idx - i as f32;
    let table = &*TANH_TABLE;
    table[i] * (1.0 - frac) + table[i + 1] * frac
}

static SILERO_MODEL: Lazy<Arc<SileroModel>> = Lazy::new(|| {
    let model = SileroModel::new().expect("Failed to load Silero model");
    Arc::new(model)
});

pub struct SileroModel {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,

    enc0: Conv1dLayer,
    enc1: Conv1dLayer,
    enc2: Conv1dLayer,
    enc3: Conv1dLayer,

    lstm_w_ih: Vec<f32>,
    lstm_w_hh: Vec<f32>,
    lstm_b_ih: Vec<f32>,
    lstm_b_hh: Vec<f32>,

    out_layer: Conv1dLayer,
}

impl SileroModel {
    pub fn new() -> Result<Self> {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(STFT_WINDOW_SIZE);

        let window = super::utils::generate_hann_window(STFT_WINDOW_SIZE, true);

        let mut model = Self {
            fft,
            window,

            enc0: Conv1dLayer::new(129, 128, 3, 1, 1, true),
            enc1: Conv1dLayer::new(128, 64, 3, 2, 1, true), // stride=2, not 1
            enc2: Conv1dLayer::new(64, 64, 3, 2, 1, true),
            enc3: Conv1dLayer::new(64, 128, 3, 1, 1, true), // stride=1, not 2
            lstm_w_ih: vec![],
            lstm_w_hh: vec![],
            lstm_b_ih: vec![],
            lstm_b_hh: vec![],
            out_layer: Conv1dLayer::new(128, 1, 1, 1, 0, false), // 1x1 conv
        };

        model.load_weights()?;

        Ok(model)
    }

    pub fn load_weights(&mut self) -> Result<()> {
        const WEIGHTS: &[u8] = include_bytes!("silero_weights.bin");
        self.load_from_bytes(WEIGHTS)?;
        Ok(())
    }

    pub fn load_from_bytes(&mut self, buffer: &[u8]) -> Result<()> {
        let mut offset = 0;

        // Helper to read u32
        let read_u32 = |offset: &mut usize, buf: &[u8]| -> u32 {
            let val = u32::from_le_bytes(buf[*offset..*offset + 4].try_into().unwrap());
            *offset += 4;
            val
        };

        let num_tensors = read_u32(&mut offset, buffer);

        for _ in 0..num_tensors {
            let name_len = read_u32(&mut offset, buffer) as usize;
            let name_bytes = &buffer[offset..offset + name_len];
            let name = std::str::from_utf8(name_bytes)?.to_string();
            offset += name_len;

            let shape_len = read_u32(&mut offset, buffer) as usize;
            let mut shape = Vec::new();
            for _ in 0..shape_len {
                shape.push(read_u32(&mut offset, buffer) as usize);
            }

            let data_len = read_u32(&mut offset, buffer) as usize;
            let data_bytes = &buffer[offset..offset + data_len];
            let data_f32: Vec<f32> = data_bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            offset += data_len;

            // Assign weights
            if name == "enc0_weight" {
                self.enc0.load_weights(data_f32);
            } else if name == "enc0_bias" {
                self.enc0.load_bias(data_f32);
            } else if name == "enc1_weight" {
                self.enc1.load_weights(data_f32);
            } else if name == "enc1_bias" {
                self.enc1.load_bias(data_f32);
            } else if name == "enc2_weight" {
                self.enc2.load_weights(data_f32);
            } else if name == "enc2_bias" {
                self.enc2.load_bias(data_f32);
            } else if name == "enc3_weight" {
                self.enc3.load_weights(data_f32);
            } else if name == "enc3_bias" {
                self.enc3.load_bias(data_f32);
            } else if name == "lstm0_w_ih" {
                let mut transformed = vec![0.0; data_f32.len()];
                let h = HIDDEN_SIZE;
                for i in 0..4 * h {
                    for j in 0..h {
                        transformed[j * 4 * h + i] = data_f32[i * h + j];
                    }
                }

                self.lstm_w_ih = transformed;
            } else if name == "lstm0_w_hh" {
                // Transform from [4*H, H] to [H, 4*H]
                let mut transformed = vec![0.0; data_f32.len()];
                let h = HIDDEN_SIZE;
                for i in 0..4 * h {
                    for j in 0..h {
                        transformed[j * 4 * h + i] = data_f32[i * h + j];
                    }
                }
                self.lstm_w_hh = transformed;
            } else if name == "lstm0_b_ih" {
                self.lstm_b_ih = data_f32;
            } else if name == "lstm0_b_hh" {
                self.lstm_b_hh = data_f32;
            } else if name == "out_weight" {
                self.out_layer.load_weights(data_f32);
            } else if name == "out_bias" {
                self.out_layer.load_bias(data_f32);
            }
        }

        Ok(())
    }
}

pub struct SileroSession {
    // State: h, c. Shape [2, 1, 128] -> We flatten to [2, 128]
    h: Vec<Vec<f32>>,
    c: Vec<Vec<f32>>,

    // Buffers for forward pass
    buf_fft_input: Vec<f32>,
    buf_fft_output: Vec<realfft::num_complex::Complex<f32>>,
    buf_fft_scratch: Vec<realfft::num_complex::Complex<f32>>,

    buf_enc0_out: Vec<f32>,
    buf_enc1_out: Vec<f32>,
    buf_enc2_out: Vec<f32>,
    buf_enc3_out: Vec<f32>,

    // Temp buffers
    buf_mag: Vec<f32>,
    buf_gates: Vec<f32>,
    buf_lstm_input: Vec<f32>,
    buf_chunk_f32: Vec<f32>,
    buf_context: Vec<f32>,      // Store last 64 samples from previous chunk
    buf_with_context: Vec<f32>, // 64 context + 512 chunk = 576 samples
    buf_padded: Vec<f32>,       // 576 + 64 padding = 640 samples for STFT

    buffer: Vec<i16>,
    current_timestamp: u64,
    processed_samples: u64,
    initialized_timestamp: bool,
}

impl SileroSession {
    pub fn new(fft: &dyn RealToComplex<f32>) -> Self {
        let fft_scratch_len = fft.get_scratch_len();
        let fft_output_len = STFT_WINDOW_SIZE / 2 + 1;

        Self {
            h: vec![vec![0.0; HIDDEN_SIZE]; 2],
            c: vec![vec![0.0; HIDDEN_SIZE]; 2],

            buf_fft_input: vec![0.0; STFT_WINDOW_SIZE],
            buf_fft_output: vec![realfft::num_complex::Complex::new(0.0, 0.0); fft_output_len],
            buf_fft_scratch: vec![realfft::num_complex::Complex::new(0.0, 0.0); fft_scratch_len],

            buf_enc0_out: vec![0.0; 128 * 4], // stride=1, input 4 -> output 4
            buf_enc1_out: vec![0.0; 64 * 2],  // stride=2, input 4 -> output 2
            buf_enc2_out: vec![0.0; 64 * 1],  // stride=2, input 2 -> output 1
            buf_enc3_out: vec![0.0; 128 * 1], // stride=1, input 1 -> output 1

            buf_mag: vec![0.0; 129 * 4],
            buf_gates: vec![0.0; 4 * HIDDEN_SIZE],
            buf_lstm_input: vec![0.0; HIDDEN_SIZE],
            buf_chunk_f32: vec![0.0; CHUNK_SIZE],
            buf_context: vec![0.0; CONTEXT_SIZE],
            buf_with_context: vec![0.0; CONTEXT_SIZE + CHUNK_SIZE],
            buf_padded: vec![0.0; CONTEXT_SIZE + CHUNK_SIZE + STFT_PADDING],

            buffer: Vec::with_capacity(CHUNK_SIZE),
            current_timestamp: 0,
            processed_samples: 0,
            initialized_timestamp: false,
        }
    }
}

pub struct TinySilero {
    config: VADOption,
    model: Arc<SileroModel>,
    session: SileroSession,
}

// removed dot_product definitions as they are no longer used by layout-optimized loops

struct Conv1dLayer {
    weights: Vec<f32>,      // [in_c, k, out_c]
    bias: Option<Vec<f32>>, // [out_c]
    in_channels: usize,
    out_channels: usize,
    kernel_size: usize,
    stride: usize,
    padding: usize,
    dilation: usize,
    relu: bool,
}

impl Conv1dLayer {
    fn new(
        in_channels: usize,
        out_channels: usize,
        kernel_size: usize,
        stride: usize,
        padding: usize,
        relu: bool,
    ) -> Self {
        Self {
            weights: vec![],
            bias: None,
            in_channels,
            out_channels,
            kernel_size,
            stride,
            padding,
            dilation: 1,
            relu,
        }
    }

    fn load_weights(&mut self, w: Vec<f32>) {
        assert_eq!(
            w.len(),
            self.out_channels * self.in_channels * self.kernel_size
        );
        // Transform from [OC, IC, K] to [IC, K, OC] for better SIMD (broadcasting OC)
        let mut transformed = vec![0.0; w.len()];
        for oc in 0..self.out_channels {
            for ic in 0..self.in_channels {
                for k in 0..self.kernel_size {
                    let old_idx = (oc * self.in_channels + ic) * self.kernel_size + k;
                    let new_idx = (ic * self.kernel_size + k) * self.out_channels + oc;
                    transformed[new_idx] = w[old_idx];
                }
            }
        }
        self.weights = transformed;
    }

    fn load_bias(&mut self, b: Vec<f32>) {
        assert_eq!(b.len(), self.out_channels);
        self.bias = Some(b);
    }

    fn forward(&self, input: &[f32], input_len: usize, output: &mut [f32]) {
        if self.weights.is_empty() {
            panic!(
                "Conv1dLayer weights not loaded! in_channels={}, out_channels={}",
                self.in_channels, self.out_channels
            );
        }
        let output_len =
            (input_len + 2 * self.padding - self.dilation * (self.kernel_size - 1) - 1)
                / self.stride
                + 1;

        // Initialize output with bias (Layout: [T, OC])
        if let Some(bias) = &self.bias {
            for t in 0..output_len {
                let start = t * self.out_channels;
                output[start..start + self.out_channels].copy_from_slice(bias);
            }
        } else {
            output.fill(0.0);
        }

        // Weight Layout: [IC, K, OC]
        // Input Layout: [T, IC]
        // Output Layout: [T, OC]
        for t in 0..output_len {
            let t_stride = t * self.stride;
            let out_start = t * self.out_channels;

            for ic in 0..self.in_channels {
                let in_ic_base = ic; // Input is [T, IC]
                let weight_ic_base = ic * self.kernel_size * self.out_channels;

                for k in 0..self.kernel_size {
                    let input_t = (t_stride + k) as isize - self.padding as isize;

                    if input_t >= 0 && input_t < input_len as isize {
                        let x = input[input_t as usize * self.in_channels + in_ic_base];
                        if x == 0.0 {
                            continue;
                        }

                        let w_start = weight_ic_base + k * self.out_channels;
                        let weight_slice = &self.weights[w_start..w_start + self.out_channels];
                        let out_slice = &mut output[out_start..out_start + self.out_channels];

                        super::simd::vec_fma(out_slice, weight_slice, x);
                    }
                }
            }
        }

        if self.relu {
            for x in output.iter_mut() {
                if *x < 0.0 {
                    *x = 0.0;
                }
            }
        }
    }
}

impl TinySilero {
    pub fn new(config: VADOption) -> Result<Self> {
        let model = Arc::clone(&SILERO_MODEL);
        let session = SileroSession::new(model.fft.as_ref());

        Ok(Self {
            config,
            model,
            session,
        })
    }

    pub fn get_last_output_logit(&self) -> f32 {
        let w = &self.model.out_layer.weights;
        let b = self.model.out_layer.bias.as_ref().unwrap()[0];
        let mut sum = b;
        for j in 0..HIDDEN_SIZE {
            let val = self.session.h[0][j];
            let val_relu = if val > 0.0 { val } else { 0.0 };
            sum += w[j] * val_relu;
        }
        sum
    }

    pub fn predict(&mut self, audio: &[f32]) -> f32 {
        let model = &self.model;
        let session = &mut self.session;

        session.buf_with_context[..CONTEXT_SIZE].copy_from_slice(&session.buf_context);
        session.buf_with_context[CONTEXT_SIZE..].copy_from_slice(audio);

        let input_len = CONTEXT_SIZE + CHUNK_SIZE; // 576
        session.buf_padded[..input_len].copy_from_slice(&session.buf_with_context);

        for i in 0..STFT_PADDING {
            let src_idx = input_len - 2 - i; // -2 to skip the last element and go backwards
            session.buf_padded[input_len + i] = session.buf_with_context[src_idx];
        }

        for t in 0..4 {
            let start = t * STFT_STRIDE;
            // Copy and Window
            for i in 0..STFT_WINDOW_SIZE {
                session.buf_fft_input[i] = session.buf_padded[start + i] * model.window[i];
            }

            // FFT
            model
                .fft
                .process_with_scratch(
                    &mut session.buf_fft_input,
                    &mut session.buf_fft_output,
                    &mut session.buf_fft_scratch,
                )
                .unwrap();

            for i in 0..129 {
                let complex = session.buf_fft_output[i];
                let mag = complex.norm(); // sqrt(re^2 + im^2)
                session.buf_mag[t * 129 + i] = mag;
            }
        }

        session
            .buf_context
            .copy_from_slice(&audio[CHUNK_SIZE - CONTEXT_SIZE..]);

        model
            .enc0
            .forward(&session.buf_mag, 4, &mut session.buf_enc0_out);

        model
            .enc1
            .forward(&session.buf_enc0_out, 4, &mut session.buf_enc1_out);

        model
            .enc2
            .forward(&session.buf_enc1_out, 2, &mut session.buf_enc2_out);

        model
            .enc3
            .forward(&session.buf_enc2_out, 1, &mut session.buf_enc3_out);

        session
            .buf_lstm_input
            .copy_from_slice(&session.buf_enc3_out);
        let (b_ih, b_hh) = (&model.lstm_b_ih, &model.lstm_b_hh);

        session.buf_gates.copy_from_slice(b_ih);
        for (g, &b) in session.buf_gates.iter_mut().zip(b_hh.iter()) {
            *g += b;
        }

        for j in 0..HIDDEN_SIZE {
            let x = session.buf_lstm_input[j];
            if x == 0.0 {
                continue;
            }
            let w_start = j * 4 * HIDDEN_SIZE;
            let weight_slice = &model.lstm_w_ih[w_start..w_start + 4 * HIDDEN_SIZE];
            super::simd::vec_fma(&mut session.buf_gates, weight_slice, x);
        }

        for j in 0..HIDDEN_SIZE {
            let h_val = session.h[0][j];
            if h_val == 0.0 {
                continue;
            }
            let w_start = j * 4 * HIDDEN_SIZE;
            let weight_slice = &model.lstm_w_hh[w_start..w_start + 4 * HIDDEN_SIZE];
            super::simd::vec_fma(&mut session.buf_gates, weight_slice, h_val);
        }

        let chunk = HIDDEN_SIZE;

        for j in 0..HIDDEN_SIZE {
            let i_gate = fast_sigmoid(session.buf_gates[0 * chunk + j]); // Position 0: I (Input)
            let f_gate = fast_sigmoid(session.buf_gates[1 * chunk + j]); // Position 1: F (Forget)
            let g_gate = fast_tanh(session.buf_gates[2 * chunk + j]); // Position 2: G (Cell/Gate)
            let o_gate = fast_sigmoid(session.buf_gates[3 * chunk + j]); // Position 3: O (Output)

            let c_new = f_gate * session.c[0][j] + i_gate * g_gate;
            let h_val = o_gate * fast_tanh(c_new);

            session.c[0][j] = c_new;
            session.h[0][j] = h_val;
        }

        let w = &model.out_layer.weights; // [128, 1, 1] -> flat 128
        let b = model.out_layer.bias.as_ref().unwrap()[0];

        let mut sum = b;
        for j in 0..HIDDEN_SIZE {
            let val = session.h[0][j];
            let val_relu = if val > 0.0 { val } else { 0.0 };
            sum += w[j] * val_relu;
        }
        let out = fast_sigmoid(sum);

        out
    }
}

impl VadEngine for TinySilero {
    fn process(&mut self, frame: &mut AudioFrame) -> Vec<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return vec![(false, frame.timestamp)],
        };

        if !self.session.initialized_timestamp {
            self.session.current_timestamp = frame.timestamp;
            self.session.initialized_timestamp = true;
        }

        self.session.buffer.extend_from_slice(samples);

        let mut results = Vec::new();

        while self.session.buffer.len() >= CHUNK_SIZE {
            let mut chunk_f32 = std::mem::take(&mut self.session.buf_chunk_f32);
            if chunk_f32.len() != CHUNK_SIZE {
                chunk_f32.resize(CHUNK_SIZE, 0.0);
            }

            for (i, sample) in self.session.buffer.iter().take(CHUNK_SIZE).enumerate() {
                chunk_f32[i] = *sample as f32 / 32768.0;
            }

            self.session.buffer.drain(..CHUNK_SIZE);
            let score = self.predict(&chunk_f32);

            self.session.buf_chunk_f32 = chunk_f32;

            let is_voice = score > self.config.voice_threshold;

            let chunk_timestamp = self.session.current_timestamp
                + (self.session.processed_samples * 1000) / (frame.sample_rate as u64);
            self.session.processed_samples += CHUNK_SIZE as u64;

            results.push((is_voice, chunk_timestamp));
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tiny_silero_load_and_run() -> Result<()> {
        let config = VADOption {
            samplerate: 16000,
            ..Default::default()
        };
        let mut vad = TinySilero::new(config)?;

        // Create dummy audio
        let audio = vec![0.0; 512];

        // Run a few times
        for i in 0..10 {
            let prob = vad.predict(&audio);
            println!("Step {}: prob = {}", i, prob);
        }

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
}
