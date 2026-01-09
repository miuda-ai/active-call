use super::{VADOption, VadEngine};
use crate::media::{AudioFrame, Samples};
use anyhow::Result;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

// Constants
const CHUNK_SIZE: usize = 512;
const HIDDEN_SIZE: usize = 128;
const STFT_WINDOW_SIZE: usize = 256;
const STFT_STRIDE: usize = 128;

#[inline(always)]
fn dot_product_128(w: &[f32], x: &[f32]) -> f32 {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("fma") {
            return unsafe { super::simd::dot_product_fma(w, x) };
        } else if is_x86_feature_detected!("avx") {
            return unsafe { super::simd::dot_product_avx(w, x) };
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        return unsafe { super::simd::dot_product_neon(w, x) };
    }

    #[allow(unreachable_code)]
    let mut sum = 0.0;
    // Unroll 8 times for better pipelining
    for k in 0..16 {
        let base = k * 8;
        sum += w[base] * x[base]
            + w[base + 1] * x[base + 1]
            + w[base + 2] * x[base + 2]
            + w[base + 3] * x[base + 3]
            + w[base + 4] * x[base + 4]
            + w[base + 5] * x[base + 5]
            + w[base + 6] * x[base + 6]
            + w[base + 7] * x[base + 7];
    }
    sum
}

#[inline(always)]
fn dot_product_256(w: &[f32], x: &[f32]) -> f32 {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("fma") {
            return unsafe { super::simd::dot_product_fma(w, x) };
        } else if is_x86_feature_detected!("avx") {
            return unsafe { super::simd::dot_product_avx(w, x) };
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        return unsafe { super::simd::dot_product_neon(w, x) };
    }

    #[allow(unreachable_code)]
    let mut sum = 0.0;
    // Unroll 8 times
    for k in 0..32 {
        let base = k * 8;
        sum += w[base] * x[base]
            + w[base + 1] * x[base + 1]
            + w[base + 2] * x[base + 2]
            + w[base + 3] * x[base + 3]
            + w[base + 4] * x[base + 4]
            + w[base + 5] * x[base + 5]
            + w[base + 6] * x[base + 6]
            + w[base + 7] * x[base + 7];
    }
    sum
}

struct Conv1dLayer {
    weights: Vec<f32>,      // [out_c, in_c, k]
    bias: Option<Vec<f32>>, // [out_c]
    in_channels: usize,
    out_channels: usize,
    kernel_size: usize,
    stride: usize,
    padding: usize,
    dilation: usize,
}

impl Conv1dLayer {
    fn new(
        in_channels: usize,
        out_channels: usize,
        kernel_size: usize,
        stride: usize,
        padding: usize,
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
        }
    }

    fn load_weights(&mut self, w: Vec<f32>) {
        assert_eq!(
            w.len(),
            self.out_channels * self.in_channels * self.kernel_size
        );
        self.weights = w;
    }

    fn load_bias(&mut self, b: Vec<f32>) {
        assert_eq!(b.len(), self.out_channels);
        self.bias = Some(b);
    }

    // Forward pass
    // Input: [in_channels, input_len] (flattened)
    // Output: [out_channels, output_len] (flattened)
    // We assume input is column-major or row-major?
    // Usually [batch, channels, time]. Here batch=1.
    // So [channels, time].
    // Let's use [time, channels] for easier iteration?
    // No, Conv1d usually works on [channels, time].
    // But for cache locality, [time, channels] might be better if we iterate time.
    // However, the weights are [out, in, k].
    // Let's stick to [channels, time] to match standard logic, or [time, channels] if it simplifies.
    // The STFT output is [3, 129] (Time, Channels).
    // The Encoder expects [Batch, Channels, Time].
    // So [129, 3].
    // Let's use [channels, time] internally.

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

        // Initialize output with bias
        if let Some(bias) = &self.bias {
            for oc in 0..self.out_channels {
                let b = bias[oc];
                let start = oc * output_len;
                let end = start + output_len;
                output[start..end].fill(b);
            }
        } else {
            output.fill(0.0);
        }

        // Optimization for STFT (Large Kernel, No Padding, Stride > 1)
        if self.padding == 0 && self.kernel_size > 16 {
            // oc -> t -> ic -> k
            // Since padding is 0, we can skip bounds checks
            for oc in 0..self.out_channels {
                let w_oc_offset = oc * self.in_channels * self.kernel_size;
                let out_idx_base = oc * output_len;

                for t in 0..output_len {
                    let t_stride = t * self.stride;
                    let mut sum = 0.0;

                    for ic in 0..self.in_channels {
                        let in_offset = ic * input_len + t_stride;
                        let w_ic_offset = w_oc_offset + ic * self.kernel_size;

                        // Vectorization friendly loop
                        let input_slice = &input[in_offset..in_offset + self.kernel_size];
                        let weight_slice =
                            &self.weights[w_ic_offset..w_ic_offset + self.kernel_size];

                        if self.kernel_size == 256 {
                            sum += dot_product_256(weight_slice, input_slice);
                        } else {
                            sum += super::simd::dot_product(weight_slice, input_slice);
                        }
                    }
                    output[out_idx_base + t] += sum;
                }
            }
            // ReLU is not needed for STFT usually?
            // Wait, Silero STFT is just a Conv1d, does it have ReLU?
            // The ONNX graph showed it's just Conv.
            // But my generic forward applies ReLU.
            // Let's check if STFT output should be ReLU'd.
            // Usually STFT is linear.
            // The ONNX graph inspection didn't show ReLU after STFT Conv.
            // It goes to Magnitude computation.
            // If I apply ReLU, I kill negative values, which ruins STFT.
            // FIX: Do NOT apply ReLU for STFT.
            // How to distinguish? STFT is the only one with kernel > 16 here.
            return;
        }

        // Optimization for Enc0 (k=3, s=1, p=1, input_len=3)
        if self.kernel_size == 3 && self.stride == 1 && self.padding == 1 && input_len == 3 {
            for oc in 0..self.out_channels {
                let w_oc_offset = oc * self.in_channels * 3;
                let out_idx_base = oc * 3; // output_len is 3

                let mut sum0 = 0.0;
                let mut sum1 = 0.0;
                let mut sum2 = 0.0;

                if let Some(bias) = &self.bias {
                    let b = bias[oc];
                    sum0 = b;
                    sum1 = b;
                    sum2 = b;
                }

                for ic in 0..self.in_channels {
                    let in_offset = ic * 3;
                    let w_offset = w_oc_offset + ic * 3;

                    // Load 3 inputs and 3 weights
                    // Using get_unchecked for speed if we are sure
                    let x0 = unsafe { *input.get_unchecked(in_offset) };
                    let x1 = unsafe { *input.get_unchecked(in_offset + 1) };
                    let x2 = unsafe { *input.get_unchecked(in_offset + 2) };

                    let w0 = unsafe { *self.weights.get_unchecked(w_offset) };
                    let w1 = unsafe { *self.weights.get_unchecked(w_offset + 1) };
                    let w2 = unsafe { *self.weights.get_unchecked(w_offset + 2) };

                    sum0 += w1 * x0 + w2 * x1;
                    sum1 += w0 * x0 + w1 * x1 + w2 * x2;
                    sum2 += w0 * x1 + w1 * x2;
                }

                // ReLU
                output[out_idx_base] = if sum0 > 0.0 { sum0 } else { 0.0 };
                output[out_idx_base + 1] = if sum1 > 0.0 { sum1 } else { 0.0 };
                output[out_idx_base + 2] = if sum2 > 0.0 { sum2 } else { 0.0 };
            }
            return;
        }

        // Optimization for Enc1 (k=3, s=2, p=1, input_len=3) -> output_len=2
        if self.kernel_size == 3 && self.stride == 2 && self.padding == 1 && input_len == 3 {
            for oc in 0..self.out_channels {
                let w_oc_offset = oc * self.in_channels * 3;
                let out_idx_base = oc * 2;

                let mut sum0 = 0.0;
                let mut sum1 = 0.0;

                if let Some(bias) = &self.bias {
                    let b = bias[oc];
                    sum0 = b;
                    sum1 = b;
                }

                for ic in 0..self.in_channels {
                    let in_offset = ic * 3;
                    let w_offset = w_oc_offset + ic * 3;

                    let x0 = unsafe { *input.get_unchecked(in_offset) };
                    let x1 = unsafe { *input.get_unchecked(in_offset + 1) };
                    let x2 = unsafe { *input.get_unchecked(in_offset + 2) };

                    let w0 = unsafe { *self.weights.get_unchecked(w_offset) };
                    let w1 = unsafe { *self.weights.get_unchecked(w_offset + 1) };
                    let w2 = unsafe { *self.weights.get_unchecked(w_offset + 2) };

                    // t=0: w[1]*x[0] + w[2]*x[1]
                    sum0 += w1 * x0 + w2 * x1;
                    // t=1: w[0]*x[1] + w[1]*x[2]
                    sum1 += w0 * x1 + w1 * x2;
                }

                output[out_idx_base] = if sum0 > 0.0 { sum0 } else { 0.0 };
                output[out_idx_base + 1] = if sum1 > 0.0 { sum1 } else { 0.0 };
            }
            return;
        }

        // Optimization for Enc2 (k=3, s=2, p=1, input_len=2) -> output_len=1
        if self.kernel_size == 3 && self.stride == 2 && self.padding == 1 && input_len == 2 {
            for oc in 0..self.out_channels {
                let w_oc_offset = oc * self.in_channels * 3;
                let out_idx_base = oc; // output_len is 1

                let mut sum0 = 0.0;

                if let Some(bias) = &self.bias {
                    sum0 = bias[oc];
                }

                for ic in 0..self.in_channels {
                    let in_offset = ic * 2;
                    let w_offset = w_oc_offset + ic * 3;

                    let x0 = unsafe { *input.get_unchecked(in_offset) };
                    let x1 = unsafe { *input.get_unchecked(in_offset + 1) };

                    let w1 = unsafe { *self.weights.get_unchecked(w_offset + 1) };
                    let w2 = unsafe { *self.weights.get_unchecked(w_offset + 2) };

                    // t=0: w[1]*x[0] + w[2]*x[1]
                    sum0 += w1 * x0 + w2 * x1;
                }

                output[out_idx_base] = if sum0 > 0.0 { sum0 } else { 0.0 };
            }
            return;
        }

        // Optimization for Enc3 (k=3, s=1, p=1, input_len=1) -> output_len=1
        if self.kernel_size == 3 && self.stride == 1 && self.padding == 1 && input_len == 1 {
            for oc in 0..self.out_channels {
                let w_oc_offset = oc * self.in_channels * 3;
                let out_idx_base = oc;

                let mut sum0 = 0.0;

                if let Some(bias) = &self.bias {
                    sum0 = bias[oc];
                }

                for ic in 0..self.in_channels {
                    let in_offset = ic; // input_len is 1
                    let w_offset = w_oc_offset + ic * 3;

                    let x0 = unsafe { *input.get_unchecked(in_offset) };
                    let w1 = unsafe { *self.weights.get_unchecked(w_offset + 1) };

                    // t=0: w[1]*x[0]
                    sum0 += w1 * x0;
                }

                output[out_idx_base] = if sum0 > 0.0 { sum0 } else { 0.0 };
            }
            return;
        }

        // Optimization for Encoder (Small Kernel k=3, Padding=1)
        if self.kernel_size == 3 {
            for oc in 0..self.out_channels {
                let w_oc_offset = oc * self.in_channels * 3;
                let out_idx_base = oc * output_len;

                for ic in 0..self.in_channels {
                    let in_offset = ic * input_len;
                    let w_ic_offset = w_oc_offset + ic * 3;

                    let w0 = self.weights[w_ic_offset];
                    let w1 = self.weights[w_ic_offset + 1];
                    let w2 = self.weights[w_ic_offset + 2];

                    for t in 0..output_len {
                        let t_stride = t * self.stride;
                        let input_t0 = (t_stride) as isize - self.padding as isize;
                        let input_t1 = (t_stride + 1) as isize - self.padding as isize;
                        let input_t2 = (t_stride + 2) as isize - self.padding as isize;

                        let mut val = 0.0;
                        if input_t0 >= 0 && input_t0 < input_len as isize {
                            val += input[in_offset + input_t0 as usize] * w0;
                        }
                        if input_t1 >= 0 && input_t1 < input_len as isize {
                            val += input[in_offset + input_t1 as usize] * w1;
                        }
                        if input_t2 >= 0 && input_t2 < input_len as isize {
                            val += input[in_offset + input_t2 as usize] * w2;
                        }
                        output[out_idx_base + t] += val;
                    }
                }
            }
        } else {
            // Fallback generic
            for oc in 0..self.out_channels {
                let w_oc_offset = oc * self.in_channels * self.kernel_size;
                let out_idx_base = oc * output_len;

                for t in 0..output_len {
                    let mut sum = 0.0;
                    let t_stride = t * self.stride;

                    for ic in 0..self.in_channels {
                        let in_offset = ic * input_len;
                        let w_ic_offset = w_oc_offset + ic * self.kernel_size;

                        for k in 0..self.kernel_size {
                            let input_t = (t_stride + k) as isize - self.padding as isize;

                            if input_t >= 0 && input_t < input_len as isize {
                                let val = input[in_offset + input_t as usize];
                                let w = self.weights[w_ic_offset + k];
                                sum += w * val;
                            }
                        }
                    }
                    output[out_idx_base + t] += sum;
                }
            }
        }

        // ReLU (Only for Encoders, not STFT)
        // We assume STFT is handled by the first block and returns early.
        for x in output.iter_mut() {
            if *x < 0.0 {
                *x = 0.0;
            }
        }
    }

    // Optimized forward for specific shapes?
    // Since the shapes are small (T=3, 2, 1), we can unroll or specialize.
    // But let's start with generic.
}

pub struct TinySilero {
    // stft: Conv1dLayer, // Replaced by FFT
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

    // State: h, c. Shape [2, 1, 128] -> We flatten to [2, 128]
    h: Vec<Vec<f32>>,
    c: Vec<Vec<f32>>,

    // Buffers
    // buf_stft_out: Vec<f32>, // Removed
    buf_fft_input: Vec<f32>,
    buf_fft_output: Vec<realfft::num_complex::Complex<f32>>,
    buf_fft_scratch: Vec<realfft::num_complex::Complex<f32>>,

    buf_enc0_out: Vec<f32>, // [128 * 3]

    buf_enc1_out: Vec<f32>, // [64 * 2]
    buf_enc2_out: Vec<f32>, // [64 * 1]
    buf_enc3_out: Vec<f32>, // [128 * 1]

    // Temp buffers
    buf_mag: Vec<f32>,        // [129 * 3]
    buf_gates: Vec<f32>,      // [4 * HIDDEN_SIZE]
    buf_lstm_input: Vec<f32>, // [HIDDEN_SIZE]
    buf_chunk_f32: Vec<f32>,  // [CHUNK_SIZE]

    config: VADOption,
    buffer: Vec<i16>,
    current_timestamp: u64,
    processed_samples: u64,
    initialized_timestamp: bool,
}

impl TinySilero {
    pub fn new(config: VADOption) -> Result<Self> {
        // Initialize FFT
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(STFT_WINDOW_SIZE);
        let fft_scratch_len = fft.get_scratch_len();
        let fft_output_len = STFT_WINDOW_SIZE / 2 + 1;

        // Initialize Hann Window
        let window = super::utils::generate_hann_window(STFT_WINDOW_SIZE, true);

        // Initialize with zeros
        let mut vad = Self {
            // stft: Conv1dLayer::new(1, 258, 256, 128, 0),
            fft,
            window,

            enc0: Conv1dLayer::new(129, 128, 3, 1, 1),
            enc1: Conv1dLayer::new(128, 64, 3, 2, 1),
            enc2: Conv1dLayer::new(64, 64, 3, 2, 1),
            enc3: Conv1dLayer::new(64, 128, 3, 1, 1),
            lstm_w_ih: vec![],
            lstm_w_hh: vec![],
            lstm_b_ih: vec![],
            lstm_b_hh: vec![],
            out_layer: Conv1dLayer::new(128, 1, 1, 1, 0), // 1x1 conv

            h: vec![vec![0.0; HIDDEN_SIZE]; 2],
            c: vec![vec![0.0; HIDDEN_SIZE]; 2],

            // buf_stft_out: vec![0.0; 258 * 3],
            buf_fft_input: vec![0.0; STFT_WINDOW_SIZE],
            buf_fft_output: vec![realfft::num_complex::Complex::new(0.0, 0.0); fft_output_len],
            buf_fft_scratch: vec![realfft::num_complex::Complex::new(0.0, 0.0); fft_scratch_len],

            buf_enc0_out: vec![0.0; 128 * 3],
            buf_enc1_out: vec![0.0; 64 * 2],
            buf_enc2_out: vec![0.0; 64 * 1],
            buf_enc3_out: vec![0.0; 128 * 1],

            buf_mag: vec![0.0; 129 * 3],
            buf_gates: vec![0.0; 4 * HIDDEN_SIZE],
            buf_lstm_input: vec![0.0; HIDDEN_SIZE],
            buf_chunk_f32: vec![0.0; CHUNK_SIZE],

            config,
            buffer: Vec::with_capacity(CHUNK_SIZE),
            current_timestamp: 0,
            processed_samples: 0,
            initialized_timestamp: false,
        };

        vad.load_weights()?;

        Ok(vad)
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

            // println!("Loading tensor: {}", name);

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
            // println!("Loading tensor: {}", name);
            if name == "stft_weight" {
                // self.stft.load_weights(data_f32);
                // Ignored, using FFT
            } else if name == "enc0_weight" {
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
            } else if name == "lstm_w_ih" {
                self.lstm_w_ih = data_f32;
            } else if name == "lstm_w_hh" {
                self.lstm_w_hh = data_f32;
            } else if name == "lstm_b_ih" {
                self.lstm_b_ih = data_f32;
            } else if name == "lstm_b_hh" {
                self.lstm_b_hh = data_f32;
            } else if name == "out_weight" {
                self.out_layer.load_weights(data_f32);
            } else if name == "out_bias" {
                self.out_layer.load_bias(data_f32);
            } else {
                println!(
                    "WARNING: Ignored tensor: {} (size: {})",
                    name,
                    data_f32.len()
                );
            }
        }

        Ok(())
    }

    pub fn predict(&mut self, audio: &[f32]) -> f32 {
        // 1. STFT via FFT
        // Input: [512]
        // Output: [129, 3] (Magnitude)

        // We have 3 frames with stride 128
        // Frame 0: 0..256
        // Frame 1: 128..384
        // Frame 2: 256..512

        for t in 0..3 {
            let start = t * STFT_STRIDE;
            // Copy and Window
            // self.buf_fft_input.copy_from_slice(&audio[start..end]); // Can't do this directly if we want to window

            for i in 0..STFT_WINDOW_SIZE {
                self.buf_fft_input[i] = audio[start + i] * self.window[i];
            }

            // FFT
            self.fft
                .process_with_scratch(
                    &mut self.buf_fft_input,
                    &mut self.buf_fft_output,
                    &mut self.buf_fft_scratch,
                )
                .unwrap();

            // Compute Magnitude and fill buf_mag
            // buf_mag layout: [129, 3] -> [ch0_t0, ch0_t1, ch0_t2, ch1_t0, ...]
            // buf_fft_output has 129 complex bins

            for i in 0..129 {
                let complex = self.buf_fft_output[i];
                let mag = complex.norm(); // sqrt(re^2 + im^2)

                // Store in interleaved format [Channels, Time]
                // Index = Channel * Time_Dim + Time_Index
                self.buf_mag[i * 3 + t] = mag;
            }
        }

        // 2. Encoder
        // Enc0: [129, 3] -> [128, 3]
        self.enc0.forward(&self.buf_mag, 3, &mut self.buf_enc0_out);

        // Enc1: [128, 3] -> [64, 2]
        self.enc1
            .forward(&self.buf_enc0_out, 3, &mut self.buf_enc1_out);

        // Enc2: [64, 2] -> [64, 1]
        self.enc2
            .forward(&self.buf_enc1_out, 2, &mut self.buf_enc2_out);

        // Enc3: [64, 1] -> [128, 1]
        self.enc3
            .forward(&self.buf_enc2_out, 1, &mut self.buf_enc3_out);

        // 3. LSTM
        // Input: [128] (from Enc3, t=0)
        // State: h, c

        // Copy initial input to temp buffer to avoid borrow checker issues and allocations
        self.buf_lstm_input.copy_from_slice(&self.buf_enc3_out);

        // Shared weights for both layers
        let (w_ih, w_hh, b_ih, b_hh) = (
            &self.lstm_w_ih,
            &self.lstm_w_hh,
            &self.lstm_b_ih,
            &self.lstm_b_hh,
        );

        self.buf_gates.fill(0.0);

        // W_ih * x + b_ih
        // x is self.buf_lstm_input
        for i in 0..4 * HIDDEN_SIZE {
            let sum = b_ih[i]
                + dot_product_128(
                    &w_ih[i * HIDDEN_SIZE..(i + 1) * HIDDEN_SIZE],
                    &self.buf_lstm_input,
                );
            self.buf_gates[i] = sum;
        }

        // W_hh * h + b_hh
        for i in 0..4 * HIDDEN_SIZE {
            let sum = b_hh[i]
                + dot_product_128(&w_hh[i * HIDDEN_SIZE..(i + 1) * HIDDEN_SIZE], &self.h[0]);
            self.buf_gates[i] += sum;
        }

        // Apply activations
        // PyTorch LSTM weights order: Input, Forget, Cell, Output (IFCO)
        let chunk = HIDDEN_SIZE;
        for j in 0..HIDDEN_SIZE {
            // IFCO order
            let i_gate = crate::media::vad::utils::sigmoid(self.buf_gates[0 * chunk + j]);
            let f_gate = crate::media::vad::utils::sigmoid(self.buf_gates[1 * chunk + j]);
            let g_gate = crate::media::vad::utils::tanh(self.buf_gates[2 * chunk + j]); // Cell
            let o_gate = crate::media::vad::utils::sigmoid(self.buf_gates[3 * chunk + j]);

            let c_new = f_gate * self.c[0][j] + i_gate * g_gate;
            let h_val = o_gate * crate::media::vad::utils::tanh(c_new);

            self.c[0][j] = c_new;
            self.h[0][j] = h_val;
        }

        // 4. Output
        // Input: h_new (from layer 1) [128] -> [128, 1]
        // Conv1d 1x1 is just Dot Product + Bias
        // Output: [1, 1]

        let w = &self.out_layer.weights; // [1, 128, 1] -> [128]
        let b = self.out_layer.bias.as_ref().unwrap()[0];

        let mut sum = b;
        for j in 0..HIDDEN_SIZE {
            // Apply ReLU (Found in ONNX graph trace: decoder/decoder/1/Relu)
            let val = self.h[0][j];
            let val_relu = if val > 0.0 { val } else { 0.0 };
            sum += w[j] * val_relu;
        }
        let out = crate::media::vad::utils::sigmoid(sum);

        out
    }
}

impl VadEngine for TinySilero {
    fn process(&mut self, frame: &mut AudioFrame) -> Vec<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return vec![(false, frame.timestamp)],
        };

        if !self.initialized_timestamp {
            self.current_timestamp = frame.timestamp;
            self.initialized_timestamp = true;
        }

        self.buffer.extend_from_slice(samples);

        let mut results = Vec::new();

        while self.buffer.len() >= CHUNK_SIZE {
            // Swap out buffer to satisfy borrow checker
            let mut chunk_f32 = std::mem::take(&mut self.buf_chunk_f32);
            // Ensure capacity/length (should be preserved across calls)
            if chunk_f32.len() != CHUNK_SIZE {
                chunk_f32.resize(CHUNK_SIZE, 0.0);
            }

            // Convert to f32 without allocation
            for (i, sample) in self.buffer.iter().take(CHUNK_SIZE).enumerate() {
                chunk_f32[i] = *sample as f32 / 32768.0;
            }

            // Remove processed samples
            self.buffer.drain(..CHUNK_SIZE);

            let score = self.predict(&chunk_f32);

            // Restore buffer
            self.buf_chunk_f32 = chunk_f32;

            let is_voice = score > self.config.voice_threshold;

            let chunk_timestamp = self.current_timestamp
                + (self.processed_samples * 1000) / (frame.sample_rate as u64);
            self.processed_samples += CHUNK_SIZE as u64;

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
}
