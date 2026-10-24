use super::{VADOption, VadEngine, simd, utils};
use crate::media::{AudioFrame, Samples};
use anyhow::Result;
use fearless_simd::{Simd, dispatch};
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::{Arc, LazyLock};

// Constants
const CHUNK_SIZE: usize = 512;
const HIDDEN_SIZE: usize = 128;
const STFT_WINDOW_SIZE: usize = 256;
const STFT_STRIDE: usize = 128;
const CONTEXT_SIZE: usize = 64; // Context from previous chunk for STFT continuity
const STFT_PADDING: usize = 64; // Right padding for STFT (ReflectionPad1d)
/// Over long stretches without speech (hold music, tones, line noise) the LSTM's
/// state can settle into hearing no speech at all, and stay there long after
/// they end; a state that heard none for this long is reset, as a fresh one
/// would be.
const RESET_AFTER_QUIET_CHUNKS: usize = 10 * 16000 / CHUNK_SIZE; // 10 s
/// A chunk is quiet below the voice threshold less this, as silero-vad's
/// `neg_threshold` is.
const QUIET_MARGIN: f32 = 0.15;
const MAX_CONV_ROWS: usize = 129 * 3; // largest in_channels * kernel_size (enc0)
const MAX_CONV_STEPS: usize = 4; // largest conv output length (enc0)

// f32::exp/tanh aren't const fn on stable, so the LUTs below use a
// hand-rolled const exp (range reduction + Taylor series in f64) to let
// SIGMOID_TABLE/TANH_TABLE be computed at compile time. Verified 0 ULP
// error vs a f64 exp/tanh reference across all 1024 entries
const fn const_exp2_frac(f: f64) -> f64 {
    // Taylor series of 2^f = e^(f*ln2), f in [-0.5, 0.5]
    let y = f * std::f64::consts::LN_2;
    let mut term = 1.0f64;
    let mut sum = 1.0f64;
    let mut n = 1u32;
    while n <= 15 {
        term = term * y / (n as f64);
        sum += term;
        n += 1;
    }
    sum
}

const fn const_exp(x: f64) -> f64 {
    let y = x * std::f64::consts::LOG2_E;
    let n = y.trunc();
    let mut f = y - n;
    let mut n = n;
    // keep f in [-0.5, 0.5] for the Taylor series to stay accurate
    if f > 0.5 {
        f -= 1.0;
        n += 1.0;
    } else if f < -0.5 {
        f += 1.0;
        n -= 1.0;
    }
    let pow2n = f64::from_bits(((n as i64 + 1023) as u64) << 52);
    pow2n * const_exp2_frac(f)
}

const fn const_sigmoid(x: f64) -> f32 {
    (1.0 / (1.0 + const_exp(-x))) as f32
}

const fn const_tanh(x: f64) -> f32 {
    let e2x = const_exp(2.0 * x);
    ((e2x - 1.0) / (e2x + 1.0)) as f32
}

const fn build_sigmoid_table() -> [f32; 1024] {
    let mut table = [0.0f32; 1024];
    let mut i = 0usize;
    while i < 1024 {
        let x = -8.0 + (i as f64) * (16.0 / 1023.0);
        table[i] = const_sigmoid(x);
        i += 1;
    }
    table
}

const fn build_tanh_table() -> [f32; 1024] {
    let mut table = [0.0f32; 1024];
    let mut i = 0usize;
    while i < 1024 {
        let x = -5.0 + (i as f64) * (10.0 / 1023.0);
        table[i] = const_tanh(x);
        i += 1;
    }
    table
}

static SIGMOID_TABLE: [f32; 1024] = build_sigmoid_table();
static TANH_TABLE: [f32; 1024] = build_tanh_table();

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
    let table = &SIGMOID_TABLE;
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
    let table = &TANH_TABLE;
    table[i] * (1.0 - frac) + table[i + 1] * frac
}

static SILERO_MODEL: LazyLock<Arc<SileroModel>> = LazyLock::new(|| {
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

    lstm_w_ih: QWeights,
    lstm_w_hh: QWeights,
    lstm_b_ih: Vec<f32>,
    lstm_b_hh: Vec<f32>,

    out_layer: Conv1dLayer,
}

impl SileroModel {
    pub fn new() -> Result<Self> {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(STFT_WINDOW_SIZE);

        let window = utils::generate_hann_window(STFT_WINDOW_SIZE, true);

        let mut model = Self {
            fft,
            window,

            enc0: Conv1dLayer::new(129, 128, 3, 1, 1, true),
            enc1: Conv1dLayer::new(128, 64, 3, 2, 1, true), // stride=2, not 1
            enc2: Conv1dLayer::new(64, 64, 3, 2, 1, true),
            enc3: Conv1dLayer::new(64, 128, 3, 1, 1, true), // stride=1, not 2
            lstm_w_ih: QWeights::default(),
            lstm_w_hh: QWeights::default(),
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
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
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
                // Transform from [4*H, H] to [H, 4*H]
                let mut transformed = vec![0.0; data_f32.len()];
                let h = HIDDEN_SIZE;
                for i in 0..4 * h {
                    for j in 0..h {
                        transformed[j * 4 * h + i] = data_f32[i * h + j];
                    }
                }

                self.lstm_w_ih = QWeights::quantize(&transformed, 4 * h);
            } else if name == "lstm0_w_hh" {
                // Transform from [4*H, H] to [H, 4*H]
                let mut transformed = vec![0.0; data_f32.len()];
                let h = HIDDEN_SIZE;
                for i in 0..4 * h {
                    for j in 0..h {
                        transformed[j * 4 * h + i] = data_f32[i * h + j];
                    }
                }
                self.lstm_w_hh = QWeights::quantize(&transformed, 4 * h);
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
    last_score: Option<f32>,
    /// Quiet chunks in a row, up to `RESET_AFTER_QUIET_CHUNKS`.
    quiet_chunks: usize,
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
            last_score: None,
            quiet_chunks: 0,
        }
    }
}

/// Pure-Rust Silero VAD (16 kHz, 512-sample chunks) with SIMD-accelerated inference.
pub struct TinySilero {
    config: VADOption,
    model: Arc<SileroModel>,
    session: SileroSession,
}

/// Weights stored as i16 with one f32 scale per output channel (`value = q * scale`).
///
/// Weight traffic dominates inference time, so halving it beats the extra convert; i16 per
/// channel keeps the error ~256x below int8 (relative error ~3e-5 of the channel's max).
#[derive(Default)]
struct QWeights {
    q: Vec<i16>,
    scales: Vec<f32>,
    /// Unquantized weights, kept so tests can build an exact f32 reference.
    #[cfg(test)]
    exact: Vec<f32>,
}

impl QWeights {
    /// Quantizes `w`, where the output channel of element `i` is `i % oc`.
    fn quantize(w: &[f32], oc: usize) -> Self {
        let mut max_abs = vec![0.0f32; oc];
        for (i, &v) in w.iter().enumerate() {
            max_abs[i % oc] = max_abs[i % oc].max(v.abs());
        }
        let scales: Vec<f32> = max_abs.iter().map(|m| m / i16::MAX as f32).collect();
        let q = w
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let scale = scales[i % oc];
                if scale > 0.0 {
                    (v / scale).round() as i16
                } else {
                    0
                }
            })
            .collect();
        Self {
            q,
            scales,
            #[cfg(test)]
            exact: w.to_vec(),
        }
    }

    /// Dequantized value of element `i`.
    fn get(&self, i: usize, oc: usize) -> f32 {
        self.q[i] as f32 * self.scales[i % oc]
    }
}

struct Conv1dLayer {
    weights: QWeights,      // [in_c, k, out_c]
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
            weights: QWeights::default(),
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
        self.weights = QWeights::quantize(&transformed, self.out_channels);
    }

    fn load_bias(&mut self, b: Vec<f32>) {
        assert_eq!(b.len(), self.out_channels);
        self.bias = Some(b);
    }

    /// Adds the convolution of `input` into `output` (already bias-initialized) for `T` output timesteps.
    #[inline(always)]
    fn accumulate<S: Simd, const T: usize, const U: usize>(
        &self,
        simd: S,
        input: &[f32],
        input_len: usize,
        output: &mut [f32],
    ) {
        assert!(self.in_channels * self.kernel_size <= MAX_CONV_ROWS);

        // xs[r * T + t]: the input feeding weight row r = (ic, k) at output timestep t, zero in
        // the padding. Input is [T_in, IC], so for a fixed (k, t) the channels are contiguous.
        let mut xs = [0.0f32; MAX_CONV_ROWS * MAX_CONV_STEPS];
        for k in 0..self.kernel_size {
            for t in 0..T {
                let input_t = (t * self.stride + k) as isize - self.padding as isize;
                if input_t < 0 || input_t >= input_len as isize {
                    continue;
                }
                let src = &input[input_t as usize * self.in_channels..][..self.in_channels];
                for (ic, &x) in src.iter().enumerate() {
                    xs[(ic * self.kernel_size + k) * T + t] = x;
                }
            }
        }

        // Compact in place to the rows that have any non-zero input: rows[i] = r and
        // xs[i * T..][..T] its inputs (the write index never passes the read index).
        let mut rows = [0usize; MAX_CONV_ROWS];
        let mut n_rows = 0;
        for r in 0..self.in_channels * self.kernel_size {
            let row: [f32; T] = core::array::from_fn(|t| xs[r * T + t]);
            if row.iter().any(|&x| x != 0.0) {
                xs[n_rows * T..(n_rows + 1) * T].copy_from_slice(&row);
                rows[n_rows] = r;
                n_rows += 1;
            }
        }

        simd::fma_rows_multi::<S, T, U>(
            simd,
            output,
            self.out_channels,
            &self.weights.q,
            &self.weights.scales,
            &rows[..n_rows],
            &xs[..n_rows * T],
        );
    }

    #[inline(always)]
    fn forward<S: Simd>(&self, simd: S, input: &[f32], input_len: usize, output: &mut [f32]) {
        if self.weights.q.is_empty() {
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
        // Every timestep is accumulated in one pass so each weight row is read once.
        match output_len {
            1 => self.accumulate::<S, 1, 8>(simd, input, input_len, output),
            2 => self.accumulate::<S, 2, 4>(simd, input, input_len, output),
            4 => self.accumulate::<S, 4, 2>(simd, input, input_len, output),
            n => panic!("unsupported conv output length {n}"),
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

    /// Clears the model's recurrent state, as at the start of a stream; the
    /// buffered audio and timestamps are kept.
    pub fn reset_state(&mut self) {
        for row in self.session.h.iter_mut().chain(self.session.c.iter_mut()) {
            row.fill(0.0);
        }
        self.session.quiet_chunks = 0;
    }

    /// Probability from the most recently processed chunk, if any.
    pub fn last_probability(&self) -> Option<f32> {
        self.session.last_score
    }

    pub fn get_last_output_logit(&self) -> f32 {
        let w = &self.model.out_layer.weights;
        let b = self.model.out_layer.bias.as_ref().unwrap()[0];
        let mut sum = b;
        for j in 0..HIDDEN_SIZE {
            let val = self.session.h[0][j];
            let val_relu = if val > 0.0 { val } else { 0.0 };
            sum += w.get(j, 1) * val_relu;
        }
        sum
    }

    /// Feed 16 kHz mono PCM. Buffers internally and returns one speech
    /// probability per full 512-sample chunk consumed.
    pub fn process_samples(&mut self, samples: &[i16]) -> Vec<f32> {
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
            self.session.last_score = Some(score);

            if score < self.config.voice_threshold - QUIET_MARGIN {
                self.session.quiet_chunks += 1;
                if self.session.quiet_chunks >= RESET_AFTER_QUIET_CHUNKS {
                    self.reset_state();
                }
            } else {
                self.session.quiet_chunks = 0;
            }

            results.push(score);
        }

        results
    }

    /// Run one chunk of exactly 512 f32 samples (range -1..1) through the model.
    pub fn predict(&mut self, audio: &[f32]) -> f32 {
        // one runtime dispatch per chunk; everything below inlines into the selected target
        dispatch!(simd::level(), simd => self.predict_impl(simd, audio))
    }

    #[inline(always)]
    fn predict_impl<S: Simd>(&mut self, simd: S, audio: &[f32]) -> f32 {
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
            simd::mul_slices(
                simd,
                &mut session.buf_fft_input,
                &session.buf_padded[start..start + STFT_WINDOW_SIZE],
                &model.window,
            );

            // FFT
            model
                .fft
                .process_with_scratch(
                    &mut session.buf_fft_input,
                    &mut session.buf_fft_output,
                    &mut session.buf_fft_scratch,
                )
                .unwrap();

            simd::complex_norm(
                simd,
                &mut session.buf_mag[t * 129..(t + 1) * 129],
                &session.buf_fft_output,
            );
        }

        session
            .buf_context
            .copy_from_slice(&audio[CHUNK_SIZE - CONTEXT_SIZE..]);

        model
            .enc0
            .forward(simd, &session.buf_mag, 4, &mut session.buf_enc0_out);

        model
            .enc1
            .forward(simd, &session.buf_enc0_out, 4, &mut session.buf_enc1_out);

        model
            .enc2
            .forward(simd, &session.buf_enc1_out, 2, &mut session.buf_enc2_out);

        model
            .enc3
            .forward(simd, &session.buf_enc2_out, 1, &mut session.buf_enc3_out);

        session
            .buf_lstm_input
            .copy_from_slice(&session.buf_enc3_out);
        let (b_ih, b_hh) = (&model.lstm_b_ih, &model.lstm_b_hh);

        session.buf_gates.copy_from_slice(b_ih);
        for (g, &b) in session.buf_gates.iter_mut().zip(b_hh.iter()) {
            *g += b;
        }

        let mut rows = [(0usize, 0.0f32); HIDDEN_SIZE];

        let mut n_rows = 0;
        for j in 0..HIDDEN_SIZE {
            let x = session.buf_lstm_input[j];
            if x == 0.0 {
                continue;
            }
            rows[n_rows] = (j * 4 * HIDDEN_SIZE, x);
            n_rows += 1;
        }
        simd::fma_rows(
            simd,
            &mut session.buf_gates,
            &model.lstm_w_ih.q,
            &model.lstm_w_ih.scales,
            &rows[..n_rows],
        );

        let mut n_rows = 0;
        for j in 0..HIDDEN_SIZE {
            let h_val = session.h[0][j];
            if h_val == 0.0 {
                continue;
            }
            rows[n_rows] = (j * 4 * HIDDEN_SIZE, h_val);
            n_rows += 1;
        }
        simd::fma_rows(
            simd,
            &mut session.buf_gates,
            &model.lstm_w_hh.q,
            &model.lstm_w_hh.scales,
            &rows[..n_rows],
        );

        // Gate order in the packed pre-activations: I (input), F (forget), G (cell), O (output)
        let (i_gates, rest) = session.buf_gates.split_at(HIDDEN_SIZE);
        let (f_gates, rest) = rest.split_at(HIDDEN_SIZE);
        let (g_gates, o_gates) = rest.split_at(HIDDEN_SIZE);

        for j in 0..HIDDEN_SIZE {
            let i_gate = fast_sigmoid(i_gates[j]);
            let f_gate = fast_sigmoid(f_gates[j]);
            let g_gate = fast_tanh(g_gates[j]);
            let o_gate = fast_sigmoid(o_gates[j]);

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
            sum += w.get(j, 1) * val_relu;
        }
        fast_sigmoid(sum)
    }
}

impl VadEngine for TinySilero {
    fn last_probability(&self) -> Option<f32> {
        self.session.last_score
    }

    fn process(&mut self, frame: &mut AudioFrame) -> Vec<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return vec![(false, frame.timestamp)],
        };

        if !self.session.initialized_timestamp {
            self.session.current_timestamp = frame.timestamp;
            self.session.initialized_timestamp = true;
        }

        let scores = self.process_samples(samples);

        scores
            .into_iter()
            .map(|score| {
                let is_voice = score > self.config.voice_threshold;

                let chunk_timestamp = self.session.current_timestamp
                    + (self.session.processed_samples * 1000) / (frame.sample_rate as u64);
                self.session.processed_samples += CHUNK_SIZE as u64;

                (is_voice, chunk_timestamp)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
