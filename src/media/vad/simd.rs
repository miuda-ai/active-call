#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use std::arch::x86_64::*;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx", enable = "fma")]
pub unsafe fn vec_fma_fma(out: &mut [f32], w: &[f32], x: f32) {
    unsafe {
        let len = out.len();
        let x_vec = _mm256_set1_ps(x);
        let mut i = 0;
        while i + 8 <= len {
            let o_v = _mm256_loadu_ps(out.as_ptr().add(i));
            let w_v = _mm256_loadu_ps(w.as_ptr().add(i));
            let res = _mm256_fmadd_ps(w_v, x_vec, o_v);
            _mm256_storeu_ps(out.as_mut_ptr().add(i), res);
            i += 8;
        }
        while i < len {
            out[i] += w[i] * x;
            i += 1;
        }
    }
}

pub fn vec_fma(out: &mut [f32], w: &[f32], x: f32) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("fma") {
            unsafe { vec_fma_fma(out, w, x) };
            return;
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            let len = out.len();
            let mut i = 0;
            while i + 4 <= len {
                let o_v = vld1q_f32(out.as_ptr().add(i));
                let w_v = vld1q_f32(w.as_ptr().add(i));
                let res = vfmaq_n_f32(o_v, w_v, x);
                vst1q_f32(out.as_mut_ptr().add(i), res);
                i += 4;
            }
            while i < len {
                out[i] += w[i] * x;
                i += 1;
            }
        };
        return;
    }
    #[allow(unused)]
    for i in 0..out.len() {
        out[i] += w[i] * x;
    }
}
