use super::{VADOption, VadEngine};
use crate::media::{AudioFrame, Samples};
use anyhow::Result;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

// Constants
const SAMPLE_RATE: u32 = 16000;
const HOP_SIZE: usize = 256; // 16ms per frame
const FFT_SIZE: usize = 1024;
const WINDOW_SIZE: usize = 768;
const MEL_FILTER_BANK_NUM: usize = 40;
const FEATURE_LEN: usize = 41; // 40 mel features + 1 pitch feature
const CONTEXT_WINDOW_LEN: usize = 3;
const HIDDEN_SIZE: usize = 64;
const EPS: f32 = 1e-20;
const PRE_EMPHASIS_COEFF: f32 = 0.97;

// Feature normalization parameters
const FEATURE_MEANS: [f32; FEATURE_LEN] = [
    -8.198_236,
    -6.265_716_6,
    -5.483_818_5,
    -4.758_691_3,
    -4.417_089,
    -4.142_893,
    -3.912_850_4,
    -3.845_928,
    -3.657_090_4,
    -3.723_418_7,
    -3.876_134_2,
    -3.843_891,
    -3.690_405_1,
    -3.756_065_8,
    -3.698_696_1,
    -3.650_463,
    -3.700_468_8,
    -3.567_321_3,
    -3.498_900_2,
    -3.477_807,
    -3.458_816,
    -3.444_923_9,
    -3.401_328_6,
    -3.306_261_3,
    -3.278_556_8,
    -3.233_250_9,
    -3.198_616,
    -3.204_526_4,
    -3.208_798_6,
    -3.257_838,
    -3.381_376_7,
    -3.534_021_4,
    -3.640_868,
    -3.726_858_9,
    -3.773_731,
    -3.804_667_2,
    -3.832_901,
    -3.871_120_5,
    -3.990_593,
    -4.480_289_5,
    9.235_69e1,
];

const FEATURE_STDS: [f32; FEATURE_LEN] = [
    5.166_064,
    4.977_21,
    4.698_896,
    4.630_621_4,
    4.634_348,
    4.641_156,
    4.640_676_5,
    4.666_367,
    4.650_534_6,
    4.640_021,
    4.637_4,
    4.620_099,
    4.596_316_3,
    4.562_655,
    4.554_36,
    4.566_910_7,
    4.562_49,
    4.562_413,
    4.585_299_5,
    4.600_179_7,
    4.592_846,
    4.585_923,
    4.583_496_6,
    4.626_093,
    4.626_958,
    4.626_289_4,
    4.637_006,
    4.683_016,
    4.726_814,
    4.734_29,
    4.753_227,
    4.849_723,
    4.869_435,
    4.884_483,
    4.921_327,
    4.959_212_3,
    4.996_619,
    5.044_823_6,
    5.072_217,
    5.096_439_4,
    1.152_136_9e2,
];

pub struct TenFeatureExtractor {
    mel_filters: ndarray::Array2<f32>,
    mel_filter_ranges: Vec<(usize, usize)>,
    window: Vec<f32>,
    // FFT related fields
    rfft: Arc<dyn RealToComplex<f32>>,
    fft_scratch: Vec<realfft::num_complex::Complex<f32>>,
    fft_output: Vec<realfft::num_complex::Complex<f32>>,
    fft_input: Vec<f32>,
    power_spectrum: Vec<f32>,
    inv_stds: Vec<f32>,
}

impl TenFeatureExtractor {
    pub fn new() -> Self {
        // Generate mel filter bank
        let (mel_filters, mel_filter_ranges) = Self::generate_mel_filters();

        // Generate Hann window
        let window = super::utils::generate_hann_window(WINDOW_SIZE, false);

        // Initialize FFT
        let mut planner = RealFftPlanner::<f32>::new();
        let rfft = planner.plan_fft_forward(FFT_SIZE);
        let fft_scratch = rfft.make_scratch_vec();
        let fft_output = rfft.make_output_vec();
        let fft_input = rfft.make_input_vec();
        let power_spectrum = vec![0.0; FFT_SIZE / 2 + 1];

        // Pre-calculate inverse STDs
        let inv_stds: Vec<f32> = FEATURE_STDS.iter().map(|&std| 1.0 / (std + EPS)).collect();

        Self {
            mel_filters,
            mel_filter_ranges,
            window,
            rfft,
            fft_scratch,
            fft_output,
            fft_input,
            power_spectrum,
            inv_stds,
        }
    }

    fn generate_mel_filters() -> (ndarray::Array2<f32>, Vec<(usize, usize)>) {
        let n_bins = FFT_SIZE / 2 + 1;

        // Generate mel frequency points
        let low_mel = 2595.0_f32 * (1.0_f32 + 0.0_f32 / 700.0_f32).log10();
        let high_mel = 2595.0_f32 * (1.0_f32 + 8000.0_f32 / 700.0_f32).log10();

        let mut mel_points = Vec::new();
        for i in 0..=MEL_FILTER_BANK_NUM + 1 {
            let mel = low_mel + (high_mel - low_mel) * i as f32 / (MEL_FILTER_BANK_NUM + 1) as f32;
            mel_points.push(mel);
        }

        // Convert to Hz
        let mut hz_points = Vec::new();
        for mel in mel_points {
            let hz = 700.0_f32 * (10.0_f32.powf(mel / 2595.0_f32) - 1.0_f32);
            hz_points.push(hz);
        }

        // Convert to FFT bin indices
        let mut bin_points = Vec::new();
        for hz in hz_points {
            let bin = ((FFT_SIZE + 1) as f32 * hz / SAMPLE_RATE as f32).floor() as usize;
            bin_points.push(bin);
        }

        // Build mel filter bank
        let mut mel_filters = ndarray::Array2::<f32>::zeros((MEL_FILTER_BANK_NUM, n_bins));
        let mut ranges = Vec::with_capacity(MEL_FILTER_BANK_NUM);

        for i in 0..MEL_FILTER_BANK_NUM {
            let start = bin_points[i];
            let end = bin_points[i + 2];
            ranges.push((start, end));

            // Left slope
            for j in bin_points[i]..bin_points[i + 1] {
                if j < n_bins {
                    mel_filters[[i, j]] =
                        (j - bin_points[i]) as f32 / (bin_points[i + 1] - bin_points[i]) as f32;
                }
            }

            // Right slope
            for j in bin_points[i + 1]..bin_points[i + 2] {
                if j < n_bins {
                    mel_filters[[i, j]] = (bin_points[i + 2] - j) as f32
                        / (bin_points[i + 2] - bin_points[i + 1]) as f32;
                }
            }
        }

        (mel_filters, ranges)
    }

    pub fn extract_features(&mut self, audio_frame: &[f32]) -> ndarray::Array1<f32> {
        // Prepare FFT input buffer
        // 1. Clear buffer
        self.fft_input.fill(0.0);

        // 2. Use provided pre-emphasized audio
        let copy_len = audio_frame.len().min(WINDOW_SIZE);
        self.fft_input[..copy_len].copy_from_slice(audio_frame);

        // 3. Windowing
        for (i, sample) in self.fft_input.iter_mut().enumerate().take(copy_len) {
            *sample *= self.window[i];
        }

        // 4. FFT
        self.rfft
            .process_with_scratch(
                &mut self.fft_input,
                &mut self.fft_output,
                &mut self.fft_scratch,
            )
            .unwrap();

        // 5. Power spectrum
        let n_bins = FFT_SIZE / 2 + 1;
        let scale = 1.0 / FFT_SIZE as f32;

        // Compute power spectrum once
        // Use iterators to avoid bounds checks
        for (pow, complex) in self.power_spectrum.iter_mut().zip(self.fft_output.iter()) {
            *pow = (complex.re * complex.re + complex.im * complex.im) * scale;
        }

        // Mel filter bank features
        let mut mel_features = ndarray::Array1::<f32>::zeros(MEL_FILTER_BANK_NUM);

        for i in 0..MEL_FILTER_BANK_NUM {
            let (start, end) = self.mel_filter_ranges[i];
            let valid_end = end.min(n_bins);

            let mut sum = 0.0;
            if start < valid_end {
                // Use slices for dot product to enable vectorization
                let filter_row = self.mel_filters.row(i);
                // Safety: we know the row is contiguous because we created it that way
                // and we haven't modified layout.
                if let Some(filter_slice) = filter_row.as_slice() {
                    let filter_sub = &filter_slice[start..valid_end];
                    let power_sub = &self.power_spectrum[start..valid_end];

                    // This dot product should be auto-vectorized
                    sum = super::simd::dot_product(filter_sub, power_sub);
                } else {
                    // Fallback if not contiguous (should not happen)
                    for j in start..valid_end {
                        sum += self.mel_filters[[i, j]] * self.power_spectrum[j];
                    }
                }
            }
            mel_features[i] = (sum + EPS).ln();
        }

        // Simple pitch estimation (using 0 as in Python code)
        let pitch_freq = 0.0;

        // Combine features
        let mut features = ndarray::Array1::<f32>::zeros(FEATURE_LEN);
        features
            .slice_mut(ndarray::s![..MEL_FILTER_BANK_NUM])
            .assign(&mel_features);
        features[MEL_FILTER_BANK_NUM] = pitch_freq;

        // Feature normalization
        // Use pre-calculated inverse STDs and iterators
        for (feat, (&mean, &inv_std)) in features
            .iter_mut()
            .zip(FEATURE_MEANS.iter().zip(self.inv_stds.iter()))
        {
            *feat = (*feat - mean) * inv_std;
        }

        features
    }
}

// 3D Tensor (H, W, C)
#[derive(Clone, Debug)]
struct Tensor3D {
    data: Vec<f32>,
    h: usize,
    w: usize,
    c: usize,
}

impl Tensor3D {
    fn new(h: usize, w: usize, c: usize) -> Self {
        Self {
            data: vec![0.0; h * w * c],
            h,
            w,
            c,
        }
    }

    fn zeros(&mut self) {
        self.data.fill(0.0);
    }

    #[inline(always)]
    fn get(&self, y: usize, x: usize, ch: usize) -> f32 {
        // Safety: We assume caller checks bounds or we rely on Vec bounds check
        self.data[y * self.w * self.c + x * self.c + ch]
    }

    #[inline(always)]
    fn set(&mut self, y: usize, x: usize, ch: usize, val: f32) {
        self.data[y * self.w * self.c + x * self.c + ch] = val;
    }
}

// Conv2D Layer
struct Conv2dLayer {
    weights: Vec<f32>,      // [out_c, in_c/groups, kh, kw]
    bias: Option<Vec<f32>>, // [out_c]
    in_channels: usize,
    out_channels: usize,
    kernel_h: usize,
    kernel_w: usize,
    stride_h: usize,
    stride_w: usize,
    padding: [usize; 4], // [top, left, bottom, right]
    groups: usize,
}

impl Conv2dLayer {
    fn new(
        in_channels: usize,
        out_channels: usize,
        kernel_h: usize,
        kernel_w: usize,
        stride_h: usize,
        stride_w: usize,
        padding: [usize; 4],
        groups: usize,
    ) -> Self {
        Self {
            weights: vec![0.0; out_channels * (in_channels / groups) * kernel_h * kernel_w],
            bias: None,
            in_channels,
            out_channels,
            kernel_h,
            kernel_w,
            stride_h,
            stride_w,
            padding,
            groups,
        }
    }

    // Optimized forward pass with pre-allocated output buffer
    fn forward_into(&self, input: &Tensor3D, output: &mut Tensor3D) {
        let out_h = output.h;
        let out_w = output.w;

        // Optimization for Conv1_DW (3x3, s=1, p=0, in=1, out=1)
        // Input: [3, 41, 1], Output: [1, 39, 1]
        if self.in_channels == 1
            && self.out_channels == 1
            && self.kernel_h == 3
            && self.kernel_w == 3
            && self.stride_h == 1
            && self.stride_w == 1
            && self.padding == [0, 0, 0, 0]
        {
            let bias = self.bias.as_ref().map(|b| b[0]).unwrap_or(0.0);
            let w = &self.weights; // 9 elements

            // Hardcoded 3x3 convolution
            // y is always 0 because out_h=1 (input_h=3, k=3, s=1 -> (3-3)/1 + 1 = 1)
            for x in 0..out_w {
                // input x range: x to x+3
                // input y range: 0 to 3
                let mut sum = bias;

                // Unroll 3x3 kernel
                // Row 0
                sum += input.get(0, x, 0) * w[0];
                sum += input.get(0, x + 1, 0) * w[1];
                sum += input.get(0, x + 2, 0) * w[2];

                // Row 1
                sum += input.get(1, x, 0) * w[3];
                sum += input.get(1, x + 1, 0) * w[4];
                sum += input.get(1, x + 2, 0) * w[5];

                // Row 2
                sum += input.get(2, x, 0) * w[6];
                sum += input.get(2, x + 1, 0) * w[7];
                sum += input.get(2, x + 2, 0) * w[8];

                output.set(0, x, 0, sum);
            }
            return;
        }

        // Optimization for Conv1_PW (1x1, s=1, p=0, in=1, out=16)
        // Input: [1, 39, 1], Output: [1, 39, 16]
        if self.in_channels == 1
            && self.out_channels == 16
            && self.kernel_h == 1
            && self.kernel_w == 1
            && self.stride_h == 1
            && self.stride_w == 1
        {
            let w = &self.weights; // 16 elements
            let b = self.bias.as_ref(); // 16 elements

            for x in 0..out_w {
                let val = input.get(0, x, 0);

                // Unroll 16 channels
                for oc in 0..16 {
                    let bias = if let Some(bias_vec) = b {
                        bias_vec[oc]
                    } else {
                        0.0
                    };
                    let res = val * w[oc] + bias;
                    output.set(0, x, oc, res);
                }
            }
            return;
        }

        // Optimization for Conv2_DW (1x3, s=2, p=[0,1,0,1], in=16, out=16, groups=16)
        // Input: [1, 19, 16], Output: [1, 10, 16]
        if self.groups == 16
            && self.in_channels == 16
            && self.out_channels == 16
            && self.kernel_h == 1
            && self.kernel_w == 3
            && self.stride_w == 2
            && self.padding == [0, 1, 0, 1]
        {
            let w = &self.weights; // 16 * 1 * 1 * 3 = 48 elements
            let b = self.bias.as_ref();

            for c in 0..16 {
                let w_offset = c * 3;
                let w0 = w[w_offset];
                let w1 = w[w_offset + 1];
                let w2 = w[w_offset + 2];
                let bias = if let Some(bias_vec) = b {
                    bias_vec[c]
                } else {
                    0.0
                };

                // x=0: in_x = -1, 0, 1. Valid: 0, 1. (w1, w2)
                let val0 = input.get(0, 0, c);
                let val1 = input.get(0, 1, c);
                let sum0 = val0 * w1 + val1 * w2 + bias;
                output.set(0, 0, c, sum0);

                // x=1..9: in_x = 1, 3, 5, ... 17.
                // x=1: in_x_origin = 1. kx=0->1, kx=1->2, kx=2->3.
                // ...
                // x=9: in_x_origin = 17. kx=0->17, kx=1->18, kx=2->19(skip).

                // Middle loop x=1..8
                for x in 1..9 {
                    let in_x_origin = x * 2 - 1;
                    let v0 = input.get(0, in_x_origin, c);
                    let v1 = input.get(0, in_x_origin + 1, c);
                    let v2 = input.get(0, in_x_origin + 2, c);
                    let sum = v0 * w0 + v1 * w1 + v2 * w2 + bias;
                    output.set(0, x, c, sum);
                }

                // x=9: in_x_origin = 17. Valid: 17, 18. (w0, w1)
                let v0 = input.get(0, 17, c);
                let v1 = input.get(0, 18, c);
                let sum9 = v0 * w0 + v1 * w1 + bias;
                output.set(0, 9, c, sum9);
            }
            return;
        }

        // Optimization for Conv2_PW (1x1, s=1, p=0, in=16, out=16)
        // Input: [1, 10, 16], Output: [1, 10, 16]
        if self.in_channels == 16
            && self.out_channels == 16
            && self.kernel_h == 1
            && self.kernel_w == 1
            && self.stride_h == 1
            && self.stride_w == 1
            && self.groups == 1
        {
            let w = &self.weights; // 16 * 16 = 256 elements
            let b = self.bias.as_ref();

            for x in 0..out_w {
                // Pre-load input channel values for this pixel to registers (hopefully)
                let mut in_vals = [0.0; 16];
                for ic in 0..16 {
                    in_vals[ic] = input.get(0, x, ic);
                }

                for oc in 0..16 {
                    let mut sum = if let Some(bias_vec) = b {
                        bias_vec[oc]
                    } else {
                        0.0
                    };
                    let w_offset = oc * 16;

                    // Unroll dot product
                    for ic in 0..16 {
                        sum += in_vals[ic] * w[w_offset + ic];
                    }
                    output.set(0, x, oc, sum);
                }
            }
            return;
        }

        // Optimization for Conv3_DW (1x3, s=2, p=[0,1,0,1], in=16, out=16, groups=16)
        // Input: [1, 10, 16], Output: [1, 5, 16]
        if self.groups == 16
            && self.in_channels == 16
            && self.out_channels == 16
            && self.kernel_h == 1
            && self.kernel_w == 3
            && self.stride_w == 2
            && self.padding == [0, 1, 0, 1]
            && out_w == 5
        {
            let w = &self.weights;
            let b = self.bias.as_ref();

            for c in 0..16 {
                let w_offset = c * 3;
                let w0 = w[w_offset];
                let w1 = w[w_offset + 1];
                let w2 = w[w_offset + 2];
                let bias = if let Some(bias_vec) = b {
                    bias_vec[c]
                } else {
                    0.0
                };

                // x=0: in_x = -1. Valid: 0, 1. (w1, w2)
                let val0 = input.get(0, 0, c);
                let val1 = input.get(0, 1, c);
                let sum0 = val0 * w1 + val1 * w2 + bias;
                output.set(0, 0, c, sum0);

                // x=1..5: in_x = 1, 3, 5, 7.
                // Max index accessed: 7 + 2 = 9. Input width is 10 (0..9). Safe.
                for x in 1..5 {
                    let in_x_origin = x * 2 - 1;
                    let v0 = input.get(0, in_x_origin, c);
                    let v1 = input.get(0, in_x_origin + 1, c);
                    let v2 = input.get(0, in_x_origin + 2, c);
                    let sum = v0 * w0 + v1 * w1 + v2 * w2 + bias;
                    output.set(0, x, c, sum);
                }
            }
            return;
        }

        // Optimization for Conv3_PW (1x1, s=1, p=0, in=16, out=32)
        // Input: [1, 5, 16], Output: [1, 5, 32]
        if self.in_channels == 16
            && self.out_channels == 32
            && self.kernel_h == 1
            && self.kernel_w == 1
            && self.stride_h == 1
            && self.stride_w == 1
            && self.groups == 1
        {
            let w = &self.weights; // 32 * 16 = 512 elements
            let b = self.bias.as_ref();

            for x in 0..out_w {
                let mut in_vals = [0.0; 16];
                for ic in 0..16 {
                    in_vals[ic] = input.get(0, x, ic);
                }

                for oc in 0..32 {
                    let mut sum = if let Some(bias_vec) = b {
                        bias_vec[oc]
                    } else {
                        0.0
                    };
                    let w_offset = oc * 16;

                    for ic in 0..16 {
                        sum += in_vals[ic] * w[w_offset + ic];
                    }
                    output.set(0, x, oc, sum);
                }
            }
            return;
        }

        // Reset output buffer
        output.zeros();

        let in_c_per_group = self.in_channels / self.groups;
        let out_c_per_group = self.out_channels / self.groups;

        // Optimization: Check if we can use fast path (no padding, stride 1, etc)
        // But here we have padding and strides.

        // Optimization: Lift bias addition out of inner loop
        if let Some(b) = &self.bias {
            for g in 0..self.groups {
                for oc in 0..out_c_per_group {
                    let out_ch_idx = g * out_c_per_group + oc;
                    let bias_val = b[out_ch_idx];
                    // Initialize output with bias
                    for y in 0..out_h {
                        for x in 0..out_w {
                            output.set(y, x, out_ch_idx, bias_val);
                        }
                    }
                }
            }
        }

        for g in 0..self.groups {
            for oc in 0..out_c_per_group {
                let out_ch_idx = g * out_c_per_group + oc;

                // Pre-calculate weight offset for this output channel
                let w_base = out_ch_idx * (in_c_per_group * self.kernel_h * self.kernel_w);

                for y in 0..out_h {
                    let in_y_origin = (y * self.stride_h) as isize - self.padding[0] as isize;

                    for x in 0..out_w {
                        let in_x_origin = (x * self.stride_w) as isize - self.padding[1] as isize;

                        let mut sum = 0.0;

                        for ic in 0..in_c_per_group {
                            let in_ch_idx = g * in_c_per_group + ic;
                            let w_ic_base = w_base + ic * (self.kernel_h * self.kernel_w);

                            for ky in 0..self.kernel_h {
                                let in_y = in_y_origin + ky as isize;
                                if in_y >= 0 && in_y < input.h as isize {
                                    let w_ky_base = w_ic_base + ky * self.kernel_w;

                                    for kx in 0..self.kernel_w {
                                        let in_x = in_x_origin + kx as isize;

                                        if in_x >= 0 && in_x < input.w as isize {
                                            // Hot path
                                            let val =
                                                input.get(in_y as usize, in_x as usize, in_ch_idx);
                                            let w_idx = w_ky_base + kx;
                                            // Safety: w_idx is within bounds by construction
                                            let w = unsafe { *self.weights.get_unchecked(w_idx) };
                                            sum += val * w;
                                        }
                                    }
                                }
                            }
                        }

                        // Accumulate to output (which already has bias)
                        let current = output.get(y, x, out_ch_idx);
                        output.set(y, x, out_ch_idx, current + sum);
                    }
                }
            }
        }
    }
}

// MaxPool2D Layer
struct MaxPool2dLayer {
    kernel_h: usize,
    kernel_w: usize,
    stride_h: usize,
    stride_w: usize,
}

impl MaxPool2dLayer {
    fn forward_into(&self, input: &Tensor3D, output: &mut Tensor3D) {
        let out_h = output.h;
        let out_w = output.w;

        // Optimization for MaxPool (1x3, s=1x2)
        if self.kernel_h == 1 && self.kernel_w == 3 && self.stride_h == 1 && self.stride_w == 2 {
            for c in 0..input.c {
                // y is always 0
                for x in 0..out_w {
                    let in_x = x * 2;
                    // We assume valid padding so in_x+2 is within bounds
                    let v0 = input.get(0, in_x, c);
                    let v1 = input.get(0, in_x + 1, c);
                    let v2 = input.get(0, in_x + 2, c);

                    let max_v = v0.max(v1).max(v2);
                    output.set(0, x, c, max_v);
                }
            }
            return;
        }

        for c in 0..input.c {
            for y in 0..out_h {
                for x in 0..out_w {
                    let mut max_val = f32::NEG_INFINITY;

                    for ky in 0..self.kernel_h {
                        for kx in 0..self.kernel_w {
                            let in_y = y * self.stride_h + ky;
                            let in_x = x * self.stride_w + kx;
                            // MaxPool usually doesn't have padding in this model (valid padding)
                            // So we can skip bounds check if we trust output size calculation
                            let val = input.get(in_y, in_x, c);
                            if val > max_val {
                                max_val = val;
                            }
                        }
                    }
                    output.set(y, x, c, max_val);
                }
            }
        }
    }
}

// Simple Linear Layer
struct LinearLayer {
    weights: Vec<f32>, // Flattened [out_features, in_features]
    bias: Vec<f32>,    // [out_features]
    in_features: usize,
    out_features: usize,
}

impl LinearLayer {
    fn new(in_features: usize, out_features: usize) -> Self {
        // Initialize with dummy weights (or load from file)
        // For now, we initialize with zeros/randoms if we were training,
        // but here we just create the structure.
        Self {
            weights: vec![0.0; out_features * in_features],
            bias: vec![0.0; out_features],
            in_features,
            out_features,
        }
    }

    fn forward(&self, input: &[f32], output: &mut [f32]) {
        assert_eq!(input.len(), self.in_features);
        assert_eq!(output.len(), self.out_features);

        // Matrix-Vector Multiplication: y = Wx + b
        // Optimized with iterators for auto-vectorization
        for (i, out_val) in output.iter_mut().enumerate() {
            let weight_row_start = i * self.in_features;
            let weight_row = &self.weights[weight_row_start..weight_row_start + self.in_features];

            let dot_product: f32 = weight_row
                .iter()
                .zip(input.iter())
                .map(|(&w, &x)| w * x)
                .sum();

            *out_val = dot_product + self.bias[i];
        }
    }
}

// LSTM Layer
struct LstmLayer {
    input_size: usize,
    hidden_size: usize,
    // Weights: 4 * hidden_size rows (i, f, g, o)
    weight_ih: Vec<f32>, // [4 * hidden_size, input_size]
    weight_hh: Vec<f32>, // [4 * hidden_size, hidden_size]
    bias_ih: Vec<f32>,   // [4 * hidden_size]
    bias_hh: Vec<f32>,   // [4 * hidden_size]

    // Scratch buffers
    gates_buffer: Vec<f32>, // [4 * hidden_size]
}

impl LstmLayer {
    fn new(input_size: usize, hidden_size: usize) -> Self {
        Self {
            input_size,
            hidden_size,
            weight_ih: vec![0.0; 4 * hidden_size * input_size],
            weight_hh: vec![0.0; 4 * hidden_size * hidden_size],
            bias_ih: vec![0.0; 4 * hidden_size],
            bias_hh: vec![0.0; 4 * hidden_size],
            gates_buffer: vec![0.0; 4 * hidden_size],
        }
    }

    fn forward_optimized(&mut self, input: &[f32], hidden: &mut [f32], cell: &mut [f32]) {
        let h_size = self.hidden_size;

        // 1. Compute W_ih * x + b_ih for all gates (i, f, g, o)
        for i in 0..4 * h_size {
            let w_start = i * self.input_size;
            let w_row = &self.weight_ih[w_start..w_start + self.input_size];
            let dot: f32 = w_row.iter().zip(input).map(|(&w, &x)| w * x).sum();
            self.gates_buffer[i] = dot + self.bias_ih[i];
        }

        // 2. Compute W_hh * h + b_hh for all gates
        // We can add directly to gates_buffer
        for i in 0..4 * h_size {
            let w_start = i * h_size;
            let w_row = &self.weight_hh[w_start..w_start + h_size];
            let dot: f32 = w_row.iter().zip(hidden.iter()).map(|(&w, &h)| w * h).sum();
            self.gates_buffer[i] += dot + self.bias_hh[i];
        }

        // 3. Apply activations and update states
        // ONNX Gates order: i, o, f, g (c)
        for i in 0..h_size {
            let i_gate = crate::media::vad::utils::sigmoid(self.gates_buffer[i]);
            let o_gate = crate::media::vad::utils::sigmoid(self.gates_buffer[i + h_size]);
            let f_gate = crate::media::vad::utils::sigmoid(self.gates_buffer[i + 2 * h_size]);
            let g_gate = crate::media::vad::utils::tanh(self.gates_buffer[i + 3 * h_size]);

            // c_t = f_t * c_{t-1} + i_t * g_t
            cell[i] = f_gate * cell[i] + i_gate * g_gate;

            // h_t = o_t * tanh(c_t)
            hidden[i] = o_gate * crate::media::vad::utils::tanh(cell[i]);
        }
    }
}

pub struct TinyTen {
    config: VADOption,
    buffer: Vec<f32>, // Store pre-emphasized f32 samples
    pre_emphasis_prev: f32,
    current_timestamp: u64,
    processed_samples: u64,
    initialized_timestamp: bool,

    feature_extractor: TenFeatureExtractor,
    feature_buffer: ndarray::Array2<f32>,

    // Model Layers
    // Block 1
    conv1_dw: Conv2dLayer,
    conv1_pw: Conv2dLayer,
    maxpool: MaxPool2dLayer,

    // Block 2
    conv2_dw: Conv2dLayer,
    conv2_pw: Conv2dLayer,

    // Block 3
    conv3_dw: Conv2dLayer,
    conv3_pw: Conv2dLayer,

    lstm1: LstmLayer,
    lstm2: LstmLayer,
    dense1: LinearLayer,
    dense2: LinearLayer,

    // Model States
    h1: Vec<f32>,
    c1: Vec<f32>,
    h2: Vec<f32>,
    c2: Vec<f32>,

    // Scratch Buffers (Pre-allocated)
    t_input: Tensor3D,
    t_conv1_dw: Tensor3D,
    t_conv1_pw: Tensor3D,
    t_maxpool: Tensor3D,
    t_conv2_dw: Tensor3D,
    t_conv2_pw: Tensor3D,
    t_conv3_dw: Tensor3D,
    t_conv3_pw: Tensor3D,

    dense_input_buffer: Vec<f32>,
    dense1_out_buffer: Vec<f32>,

    last_score: Option<f32>,
}

const WEIGHTS_BYTES: &[u8] = include_bytes!("tiny_tenvad.bin");

impl TinyTen {
    pub fn new(config: VADOption) -> Result<Self> {
        if config.samplerate != 16000 {
            return Err(anyhow::anyhow!("TinyVad only supports 16kHz audio"));
        }

        let feature_extractor = TenFeatureExtractor::new();
        let feature_buffer = ndarray::Array2::<f32>::zeros((CONTEXT_WINDOW_LEN, FEATURE_LEN));

        // Initialize layers
        // Conv1: Input [1, 3, 41, 1]
        // DW: 3x3, stride 1, pad 0. Out: [1, 1, 39, 1]
        let conv1_dw = Conv2dLayer::new(1, 1, 3, 3, 1, 1, [0, 0, 0, 0], 1);
        // PW: 1x1, stride 1, pad 0. Out: [1, 1, 39, 16]
        let conv1_pw = Conv2dLayer::new(1, 16, 1, 1, 1, 1, [0, 0, 0, 0], 1);

        // MaxPool: 1x3, stride 1x2. Out: [1, 1, 19, 16]
        let maxpool = MaxPool2dLayer {
            kernel_h: 1,
            kernel_w: 3,
            stride_h: 1,
            stride_w: 2,
        };

        // Conv2: Input [1, 1, 19, 16]
        // DW: 1x3, stride 2x2, pad [0, 1, 0, 1]. Out: [1, 1, 10, 16]
        let conv2_dw = Conv2dLayer::new(16, 16, 1, 3, 2, 2, [0, 1, 0, 1], 16);
        // PW: 1x1, stride 1, pad 0. Out: [1, 1, 10, 16]
        let conv2_pw = Conv2dLayer::new(16, 16, 1, 1, 1, 1, [0, 0, 0, 0], 1);

        // Conv3: Input [1, 1, 10, 16]
        // DW: 1x3, stride 2x2, pad [0, 0, 0, 1]. Out: [1, 1, 5, 16]
        let conv3_dw = Conv2dLayer::new(16, 16, 1, 3, 2, 2, [0, 0, 0, 1], 16);
        // PW: 1x1, stride 1, pad 0. Out: [1, 1, 5, 16]
        let conv3_pw = Conv2dLayer::new(16, 16, 1, 1, 1, 1, [0, 0, 0, 0], 1);

        // LSTM Input size: 5 * 16 = 80.
        let lstm1 = LstmLayer::new(80, HIDDEN_SIZE);
        let lstm2 = LstmLayer::new(HIDDEN_SIZE, HIDDEN_SIZE);

        let dense1 = LinearLayer::new(HIDDEN_SIZE * 2, 32);
        let dense2 = LinearLayer::new(32, 1);

        // Pre-allocate scratch buffers
        let t_input = Tensor3D::new(CONTEXT_WINDOW_LEN, FEATURE_LEN, 1);
        let t_conv1_dw = Tensor3D::new(1, 39, 1);
        let t_conv1_pw = Tensor3D::new(1, 39, 16);
        let t_maxpool = Tensor3D::new(1, 19, 16);
        let t_conv2_dw = Tensor3D::new(1, 10, 16);
        let t_conv2_pw = Tensor3D::new(1, 10, 16);
        let t_conv3_dw = Tensor3D::new(1, 5, 16);
        let t_conv3_pw = Tensor3D::new(1, 5, 16);

        let dense_input_buffer = vec![0.0; HIDDEN_SIZE * 2];
        let dense1_out_buffer = vec![0.0; 32];

        let mut vad = Self {
            config,
            buffer: Vec::with_capacity(768),
            pre_emphasis_prev: 0.0,
            current_timestamp: 0,
            processed_samples: 0,
            initialized_timestamp: false,
            feature_extractor,
            feature_buffer,
            conv1_dw,
            conv1_pw,
            maxpool,
            conv2_dw,
            conv2_pw,
            conv3_dw,
            conv3_pw,
            lstm1,
            lstm2,
            dense1,
            dense2,
            h1: vec![0.0; HIDDEN_SIZE],
            c1: vec![0.0; HIDDEN_SIZE],
            h2: vec![0.0; HIDDEN_SIZE],
            c2: vec![0.0; HIDDEN_SIZE],
            t_input,
            t_conv1_dw,
            t_conv1_pw,
            t_maxpool,
            t_conv2_dw,
            t_conv2_pw,
            t_conv3_dw,
            t_conv3_pw,
            dense_input_buffer,
            dense1_out_buffer,
            last_score: None,
        };

        vad.load_weights_from_bytes(WEIGHTS_BYTES)?;
        Ok(vad)
    }

    pub fn predict(&mut self, audio_frame: &[f32]) -> f32 {
        // 1. Extract features
        let features = self.feature_extractor.extract_features(audio_frame);

        // 2. Update context window
        for i in 0..CONTEXT_WINDOW_LEN - 1 {
            for j in 0..FEATURE_LEN {
                self.feature_buffer[[i, j]] = self.feature_buffer[[i + 1, j]];
            }
        }
        for j in 0..FEATURE_LEN {
            self.feature_buffer[[CONTEXT_WINDOW_LEN - 1, j]] = features[j];
        }

        // 3. Prepare Input Tensor [1, 3, 41, 1]
        // H=3 (Time), W=41 (Freq), C=1
        // Reuse t_input
        for i in 0..CONTEXT_WINDOW_LEN {
            for j in 0..FEATURE_LEN {
                self.t_input.set(i, j, 0, self.feature_buffer[[i, j]]);
            }
        }

        // 4. Forward Pass
        // Block 1
        self.conv1_dw
            .forward_into(&self.t_input, &mut self.t_conv1_dw);
        self.conv1_pw
            .forward_into(&self.t_conv1_dw, &mut self.t_conv1_pw);

        // Apply Relu
        for val in self.t_conv1_pw.data.iter_mut() {
            *val = val.max(0.0);
        }

        self.maxpool
            .forward_into(&self.t_conv1_pw, &mut self.t_maxpool);

        // Block 2
        self.conv2_dw
            .forward_into(&self.t_maxpool, &mut self.t_conv2_dw);
        self.conv2_pw
            .forward_into(&self.t_conv2_dw, &mut self.t_conv2_pw);

        for val in self.t_conv2_pw.data.iter_mut() {
            *val = val.max(0.0);
        }

        // Block 3
        self.conv3_dw
            .forward_into(&self.t_conv2_pw, &mut self.t_conv3_dw);
        self.conv3_pw
            .forward_into(&self.t_conv3_dw, &mut self.t_conv3_pw);

        for val in self.t_conv3_pw.data.iter_mut() {
            *val = val.max(0.0);
        }

        // Flatten for LSTM
        // x shape should be [1, 5, 16] -> 80 elements
        let lstm_input = &self.t_conv3_pw.data;

        // LSTM 1
        self.lstm1
            .forward_optimized(lstm_input, &mut self.h1, &mut self.c1);

        // LSTM 2
        self.lstm2
            .forward_optimized(&self.h1, &mut self.h2, &mut self.c2);

        // Concat h2, h1 (Graph says concat_1 inputs: lstm2, lstm1)
        // dense_input_buffer is [h2, h1]
        let h_size = HIDDEN_SIZE;
        self.dense_input_buffer[0..h_size].copy_from_slice(&self.h2);
        self.dense_input_buffer[h_size..2 * h_size].copy_from_slice(&self.h1);

        // Dense 1
        self.dense1
            .forward(&self.dense_input_buffer, &mut self.dense1_out_buffer);
        // Relu
        for val in self.dense1_out_buffer.iter_mut() {
            *val = val.max(0.0);
        }

        // Dense 2
        let mut output = [0.0; 1];
        self.dense2.forward(&self.dense1_out_buffer, &mut output);

        let score = 1.0 / (1.0 + (-output[0]).exp()); // Sigmoid
        self.last_score = Some(score);

        score
    }

    fn load_weights_from_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let mut offset = 0;

        // Helper to read u32
        let read_u32 = |offset: &mut usize, buf: &[u8]| -> u32 {
            let val = u32::from_le_bytes(buf[*offset..*offset + 4].try_into().unwrap());
            *offset += 4;
            val
        };

        let num_tensors = read_u32(&mut offset, bytes);

        let mut weights = std::collections::HashMap::new();

        for _ in 0..num_tensors {
            let name_len = read_u32(&mut offset, bytes) as usize;
            let name_bytes = &bytes[offset..offset + name_len];
            let name = std::str::from_utf8(name_bytes)?.to_string();
            offset += name_len;

            let shape_len = read_u32(&mut offset, bytes) as usize;
            let mut shape = Vec::new();
            for _ in 0..shape_len {
                shape.push(read_u32(&mut offset, bytes));
            }

            let data_len = read_u32(&mut offset, bytes) as usize;
            let data_bytes = &bytes[offset..offset + data_len];
            let floats: Vec<f32> = data_bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            offset += data_len;

            weights.insert(name, (shape, floats));
        }

        // Assign weights
        if let Some(w) = weights.get("conv1_dw_weight") {
            self.conv1_dw.weights = w.1.clone();
        }
        if let Some(w) = weights.get("conv1_pw_weight") {
            self.conv1_pw.weights = w.1.clone();
        }
        if let Some(w) = weights.get("conv1_bias") {
            self.conv1_pw.bias = Some(w.1.clone());
        }

        if let Some(w) = weights.get("conv2_dw_weight") {
            self.conv2_dw.weights = w.1.clone();
        }
        if let Some(w) = weights.get("conv2_pw_weight") {
            self.conv2_pw.weights = w.1.clone();
        }
        if let Some(w) = weights.get("conv2_bias") {
            self.conv2_pw.bias = Some(w.1.clone());
        }

        if let Some(w) = weights.get("conv3_dw_weight") {
            self.conv3_dw.weights = w.1.clone();
        }
        if let Some(w) = weights.get("conv3_pw_weight") {
            self.conv3_pw.weights = w.1.clone();
        }
        if let Some(w) = weights.get("conv3_bias") {
            self.conv3_pw.bias = Some(w.1.clone());
        }

        if let Some(w) = weights.get("lstm1_w_ih") {
            self.lstm1.weight_ih = w.1.clone();
        }
        if let Some(w) = weights.get("lstm1_w_hh") {
            self.lstm1.weight_hh = w.1.clone();
        }
        if let Some(w) = weights.get("lstm1_bias") {
            // Split bias into ih and hh if needed, or just use as is.
            // ONNX LSTM bias is [8*H]. Our LstmLayer expects bias_ih [4*H] and bias_hh [4*H].
            // Usually first half is W_b, second half is R_b.
            let b = &w.1;
            if b.len() == 8 * HIDDEN_SIZE {
                self.lstm1.bias_ih = b[0..4 * HIDDEN_SIZE].to_vec();
                self.lstm1.bias_hh = b[4 * HIDDEN_SIZE..].to_vec();
            }
        }

        if let Some(w) = weights.get("lstm2_w_ih") {
            self.lstm2.weight_ih = w.1.clone();
        }
        if let Some(w) = weights.get("lstm2_w_hh") {
            self.lstm2.weight_hh = w.1.clone();
        }
        if let Some(w) = weights.get("lstm2_bias") {
            let b = &w.1;
            if b.len() == 8 * HIDDEN_SIZE {
                self.lstm2.bias_ih = b[0..4 * HIDDEN_SIZE].to_vec();
                self.lstm2.bias_hh = b[4 * HIDDEN_SIZE..].to_vec();
            }
        }

        if let Some(w) = weights.get("dense1_weight") {
            self.dense1.weights = w.1.clone();
        }
        if let Some(w) = weights.get("dense1_bias") {
            self.dense1.bias = w.1.clone();
        }

        if let Some(w) = weights.get("dense2_weight") {
            self.dense2.weights = w.1.clone();
        }
        if let Some(w) = weights.get("dense2_bias") {
            self.dense2.bias = w.1.clone();
        }
        Ok(())
    }
}

impl VadEngine for TinyTen {
    fn process(&mut self, frame: &mut AudioFrame) -> Vec<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return vec![(false, frame.timestamp)],
        };

        if !self.initialized_timestamp {
            self.current_timestamp = frame.timestamp;
            self.initialized_timestamp = true;
            // Pre-pad with zeros for the initial context
            // librosa center=True pads n_fft/2 (512).
            // 512 context makes the first chunk start at the end of the window.
            self.buffer.resize(512, 0.0);
        }

        // Apply pre-emphasis once to NEW samples and push to buffer
        let inv_scale = 1.0 / 32768.0;
        for &sample in samples {
            let s_f32 = sample as f32;
            let pre_emphasized = (s_f32 - PRE_EMPHASIS_COEFF * self.pre_emphasis_prev) * inv_scale;
            self.buffer.push(pre_emphasized);
            self.pre_emphasis_prev = s_f32;
        }

        let mut results = Vec::new();

        while self.buffer.len() >= WINDOW_SIZE {
            // Predict uses the full window but we only drain the hop size
            let window = self.buffer[..WINDOW_SIZE].to_vec();
            let score = self.predict(&window);

            let is_voice = score > self.config.voice_threshold;

            let chunk_timestamp =
                self.current_timestamp + (self.processed_samples * 1000) / (SAMPLE_RATE as u64);
            self.processed_samples += HOP_SIZE as u64;

            results.push((is_voice, chunk_timestamp));
            self.buffer.drain(..HOP_SIZE);
        }

        results
    }
}
