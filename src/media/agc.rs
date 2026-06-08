use crate::media::{AudioFrame, Sample, Samples, processor::Processor};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use sonora_agc2::adaptive_digital_gain_controller::{AdaptiveDigitalGainController, FrameInfo};
use sonora_agc2::common::{
    ADJACENT_SPEECH_FRAMES_THRESHOLD, FRAME_DURATION_MS, MIN_LEVEL_DBFS,
    SATURATION_PROTECTOR_INITIAL_HEADROOM_DB, float_s16_to_dbfs,
};
use sonora_agc2::limiter::Limiter;
use sonora_agc2::noise_level_estimator::NoiseLevelEstimator;
use sonora_agc2::saturation_protector::SaturationProtector;
use sonora_agc2::speech_level_estimator::{AdaptiveDigitalConfig, SpeechLevelEstimator};

// AGC2 processes audio in fixed 10ms sub-frames.
const SUB_FRAME_MS: u32 = FRAME_DURATION_MS as u32;

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct AGCOption {
    /// Target headroom below 0 dBFS in dB. AGC2 default: 5.0.
    pub headroom_db: Option<f32>,
    /// Maximum gain in dB. AGC2 default: 50.0.
    pub max_gain_db: Option<f32>,
    /// Initial gain applied before the speech level estimator becomes confident. AGC2 default: 15.0.
    pub initial_gain_db: Option<f32>,
    /// Max gain change in dB per second (attack/release rate). AGC2 default: 6.0.
    pub max_gain_change_db_per_second: Option<f32>,
    /// Noise floor cap above which AGC2 will not amplify. AGC2 default: -50.0.
    pub max_output_noise_level_dbfs: Option<f32>,
    /// Number of consecutive speech sub-frames required before allowing gain increase. AGC2 default: 12 (≈120 ms).
    pub adjacent_speech_frames_threshold: Option<i32>,
    /// Run the limiter after the adaptive gain. Default: true.
    pub enable_limiter: Option<bool>,
}

impl Default for AGCOption {
    fn default() -> Self {
        Self {
            headroom_db: None,
            max_gain_db: None,
            initial_gain_db: None,
            max_gain_change_db_per_second: None,
            max_output_noise_level_dbfs: None,
            adjacent_speech_frames_threshold: None,
            enable_limiter: None,
        }
    }
}

pub struct AutomaticGainControl {
    sample_rate: u32,
    sub_frame_samples: usize,
    level_estimator: SpeechLevelEstimator,
    saturation_protector: SaturationProtector,
    noise_estimator: NoiseLevelEstimator,
    controller: AdaptiveDigitalGainController,
    limiter: Option<Limiter>,
    f32_buf: Vec<f32>,
    leftover: Vec<i16>,
    last_speech_probability: f32,
    last_applied_gain_linear: f32,
}

impl AutomaticGainControl {
    pub fn new(sample_rate: u32, option: AGCOption) -> Result<Self> {
        let config = AdaptiveDigitalConfig {
            headroom_db: option.headroom_db.unwrap_or(5.0),
            max_gain_db: option.max_gain_db.unwrap_or(50.0),
            initial_gain_db: option.initial_gain_db.unwrap_or(15.0),
            max_gain_change_db_per_second: option.max_gain_change_db_per_second.unwrap_or(6.0),
            max_output_noise_level_dbfs: option.max_output_noise_level_dbfs.unwrap_or(-50.0),
        };
        let adjacent_threshold = option
            .adjacent_speech_frames_threshold
            .unwrap_or(ADJACENT_SPEECH_FRAMES_THRESHOLD);

        let sub_frame_samples = (sample_rate as usize * SUB_FRAME_MS as usize) / 1000;
        if sub_frame_samples == 0 || sub_frame_samples % 20 != 0 {
            anyhow::bail!(
                "sample rate {sample_rate} produces {sub_frame_samples} samples per 10ms, must be > 0 and divisible by 20"
            );
        }

        let level_estimator = SpeechLevelEstimator::new(&config, adjacent_threshold);
        let saturation_protector =
            SaturationProtector::new(SATURATION_PROTECTOR_INITIAL_HEADROOM_DB, adjacent_threshold);
        let noise_estimator = NoiseLevelEstimator::default();
        let controller = AdaptiveDigitalGainController::new(config, adjacent_threshold);
        let limiter = match option.enable_limiter.unwrap_or(true) {
            true => Some(Limiter::new(sub_frame_samples)),
            false => None,
        };

        Ok(Self {
            sample_rate,
            sub_frame_samples,
            level_estimator,
            saturation_protector,
            noise_estimator,
            controller,
            limiter,
            f32_buf: vec![0.0; sub_frame_samples],
            leftover: Vec::with_capacity(sub_frame_samples),
            last_speech_probability: 0.0,
            last_applied_gain_linear: 1.0,
        })
    }

    #[cfg(test)]
    pub(crate) fn current_gain_for_test(&self) -> f32 {
        self.last_applied_gain_linear
    }

    fn process_sub_frame(&mut self, sub_frame_i16: &mut [i16]) {
        debug_assert_eq!(sub_frame_i16.len(), self.sub_frame_samples);

        // Convert i16 -> S16-float (range [-32768, 32767]).
        for (dst, src) in self.f32_buf.iter_mut().zip(sub_frame_i16.iter()) {
            *dst = *src as f32;
        }

        let speech_probability = self.last_speech_probability;

        let mut peak_abs = 0.0_f32;
        let mut sum_sq = 0.0_f32;
        for &s in self.f32_buf.iter() {
            peak_abs = peak_abs.max(s.abs());
            sum_sq += s * s;
        }
        let rms_lin = (sum_sq / self.f32_buf.len() as f32).sqrt();
        // Clamp to satisfy SpeechLevelEstimator debug asserts on silent frames.
        let rms_dbfs = float_s16_to_dbfs(rms_lin).clamp(MIN_LEVEL_DBFS, 30.0);
        let peak_dbfs = float_s16_to_dbfs(peak_abs).clamp(MIN_LEVEL_DBFS, 30.0);

        self.level_estimator.update(rms_dbfs, speech_probability);
        self.saturation_protector.analyze(
            speech_probability,
            peak_dbfs,
            self.level_estimator.level_dbfs(),
        );

        let mono_view: [&[f32]; 1] = [&self.f32_buf];
        let noise_rms_dbfs = self.noise_estimator.analyze(&mono_view);

        let limiter_envelope_dbfs = self
            .limiter
            .as_ref()
            .map(|l| l.last_audio_level())
            .unwrap_or(MIN_LEVEL_DBFS);

        let info = FrameInfo {
            speech_probability,
            speech_level_dbfs: self.level_estimator.level_dbfs(),
            speech_level_reliable: self.level_estimator.is_confident(),
            noise_rms_dbfs,
            headroom_db: self.saturation_protector.headroom_db(),
            limiter_envelope_dbfs,
        };

        // Sample a non-zero input to estimate the applied linear gain after the
        // adaptive controller. Stored so tests can read back the gain decision
        // independently of the input level.
        let probe_idx = self
            .f32_buf
            .iter()
            .position(|&s| s.abs() > 1.0)
            .unwrap_or(0);
        let probe_in = self.f32_buf[probe_idx];

        let mut channels: [&mut [f32]; 1] = [&mut self.f32_buf];
        self.controller.process(&info, &mut channels);

        if probe_in.abs() > 1.0 {
            self.last_applied_gain_linear = self.f32_buf[probe_idx] / probe_in;
        }

        if let Some(limiter) = self.limiter.as_mut() {
            let mut channels: [&mut [f32]; 1] = [&mut self.f32_buf];
            limiter.process(&mut channels);
        }

        // Convert back to i16 with saturating cast.
        for (dst, src) in sub_frame_i16.iter_mut().zip(self.f32_buf.iter()) {
            *dst = src.clamp(i16::MIN as f32, i16::MAX as f32) as Sample;
        }
    }
}

impl Processor for AutomaticGainControl {
    fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        let samples = match &mut frame.samples {
            Samples::PCM { samples } if !samples.is_empty() => samples,
            _ => return Ok(()),
        };

        if frame.sample_rate != self.sample_rate {
            // Frame from an unexpected rate; pass-through.
            return Ok(());
        }

        if let Some(p) = frame.speech_probability {
            self.last_speech_probability = p.clamp(0.0, 1.0);
        }

        // Accumulate input into sub-frame-sized chunks. The leftover buffer
        // carries samples across frames so a 20ms input becomes two 10ms
        // AGC2 sub-frames cleanly.
        let mut work = std::mem::take(samples);
        let mut produced: Vec<i16> = Vec::with_capacity(work.len() + self.leftover.len());

        if !self.leftover.is_empty() {
            let mut combined = std::mem::take(&mut self.leftover);
            combined.append(&mut work);
            work = combined;
        }

        let mut idx = 0;
        while idx + self.sub_frame_samples <= work.len() {
            let end = idx + self.sub_frame_samples;
            self.process_sub_frame(&mut work[idx..end]);
            produced.extend_from_slice(&work[idx..end]);
            idx = end;
        }

        if idx < work.len() {
            self.leftover.extend_from_slice(&work[idx..]);
        }

        *samples = produced;
        Ok(())
    }
}
