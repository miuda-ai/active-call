use super::tokenizer::TokenDecoder;
use anyhow::{Context, Result, anyhow, ensure};
use ndarray::{Array3, Axis};
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::{DynValue, Tensor},
};
use std::{fs, path::Path};
use tracing::warn;

pub struct SensevoiceEncoder {
    session: Session,
    input_names: Vec<String>,
    decoder: TokenDecoder,
}

impl SensevoiceEncoder {
    pub fn new<P: AsRef<Path>>(
        model_path: P,
        tokens_path: P,
        intra_threads: usize,
    ) -> Result<Self> {
        let session = build_session_with_ort_cache(model_path.as_ref(), intra_threads)?;
        // Use fixed input names for SenseVoice model
        let input_names = vec![
            "x".to_string(),
            "x_length".to_string(),
            "language".to_string(),
            "text_norm".to_string(),
        ];
        let decoder = TokenDecoder::new(tokens_path)?;

        Ok(Self {
            session,
            input_names,
            decoder,
        })
    }

    pub fn run_and_decode(
        &mut self,
        feats: ndarray::ArrayView3<'_, f32>, // [B=1, T, D]
        language_id: i32,
        use_itn: bool,
    ) -> Result<String> {
        let b = feats.len_of(Axis(0));
        ensure!(b == 1, "batch=1 only");
        let t = feats.len_of(Axis(1));
        let d = feats.len_of(Axis(2));
        ensure!(d == 560, "expect feature dim 560 but got {}", d);
        ensure!(
            language_id >= 0 && language_id < 16,
            "invalid language id {language_id}"
        );

        let text_norm_idx = if use_itn { 14 } else { 15 };

        // Create tensors using (shape, data) tuple format
        let feats_owned = feats.to_owned();
        let shape = feats_owned.shape().to_vec();
        let (data, _offset) = feats_owned.into_raw_vec_and_offset();
        let input_tensor = Tensor::from_array((shape.as_slice(), data))
            .map_err(|e| anyhow!("ORT tensor error: {e}"))?;

        let len_tensor = Tensor::from_array(([1], vec![t as i32]))
            .map_err(|e| anyhow!("ORT tensor error: {e}"))?;

        let lang_tensor = Tensor::from_array(([1], vec![language_id]))
            .map_err(|e| anyhow!("ORT tensor error: {e}"))?;

        let tn_tensor = Tensor::from_array(([1], vec![text_norm_idx as i32]))
            .map_err(|e| anyhow!("ORT tensor error: {e}"))?;

        let mut x_val = Some(input_tensor.into_dyn());
        let mut len_val = Some(len_tensor.into_dyn());
        let mut lang_val = Some(lang_tensor.into_dyn());
        let mut tn_val = Some(tn_tensor.into_dyn());

        let mut inputs: Vec<(String, DynValue)> = Vec::with_capacity(self.input_names.len());
        for name in &self.input_names {
            let value = match name.as_str() {
                "x" => x_val
                    .take()
                    .ok_or_else(|| anyhow!("duplicate tensor binding for input 'x'"))?,
                "x_length" => len_val
                    .take()
                    .ok_or_else(|| anyhow!("duplicate tensor binding for input 'x_length'"))?,
                "language" => lang_val
                    .take()
                    .ok_or_else(|| anyhow!("duplicate tensor binding for input 'language'"))?,
                "text_norm" => tn_val
                    .take()
                    .ok_or_else(|| anyhow!("duplicate tensor binding for input 'text_norm'"))?,
                other => anyhow::bail!("unexpected encoder input '{other}'"),
            };
            inputs.push((name.clone(), value));
        }

        let outputs = self
            .session
            .run(inputs)
            .map_err(|e| anyhow!("ORT run error: {e}"))?;
        let logits_value = &outputs[0];
        let (shape, data) = logits_value
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow!("ORT extract tensor error: {e}"))?;
        let dims: Vec<usize> = shape.iter().map(|d| *d as usize).collect();
        ensure!(dims.len() == 3, "unexpected logits rank: {:?}", dims);
        ensure!(dims[0] == 1, "expect batch=1 but got {}", dims[0]);
        let logits = Array3::from_shape_vec((dims[0], dims[1], dims[2]), data.to_vec())?;
        let ids = argmax_and_unique(logits.index_axis(Axis(0), 0));
        Ok(self.decoder.decode_ids(&ids))
    }
}

fn build_session_with_ort_cache(model_path: &Path, intra_threads: usize) -> Result<Session> {
    let ort_path = model_path.with_extension("ort");

    if ort_path.exists() {
        let session_attempt = Session::builder()
            .map_err(|e| anyhow!("ORT session builder error: {e}"))?
            .with_intra_threads(intra_threads)
            .map_err(|e| anyhow!("ORT intra threads error: {e}"))?
            .commit_from_file(&ort_path);

        match session_attempt {
            Ok(session) => return Ok(session),
            Err(err) => {
                warn!(
                    ort = %ort_path.display(),
                    model = %model_path.display(),
                    error = %err,
                    "failed to load cached ORT graph, regenerating"
                );
                let _ = fs::remove_file(&ort_path);
            }
        }
    }

    let builder = Session::builder()
        .map_err(|e| anyhow!("ORT session builder error: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level2)
        .map_err(|e| anyhow!("ORT optimization level error: {e}"))?
        .with_intra_threads(intra_threads)
        .map_err(|e| anyhow!("ORT intra threads error: {e}"))?;

    if let Ok(builder_with_cache) = builder.with_optimized_model_path(&ort_path) {
        match builder_with_cache.commit_from_file(model_path) {
            Ok(session) => return Ok(session),
            Err(err) => {
                warn!(
                    ort = %ort_path.display(),
                    model = %model_path.display(),
                    error = %err,
                    "failed to build session with ORT cache, retrying without cache"
                );
                let _ = fs::remove_file(&ort_path);
            }
        }
    }

    let fallback_builder = Session::builder()
        .map_err(|e| anyhow!("ORT session builder error: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level2)
        .map_err(|e| anyhow!("ORT optimization level error: {e}"))?
        .with_intra_threads(intra_threads)
        .map_err(|e| anyhow!("ORT intra threads error: {e}"))?;

    let model_bytes = fs::read(model_path)
        .with_context(|| format!("read encoder model {}", model_path.display()))?;
    fallback_builder
        .commit_from_memory(&model_bytes)
        .map_err(|e| anyhow!("ORT load model error: {e}"))
}

fn argmax_and_unique(logits: ndarray::ArrayView2<'_, f32>) -> Vec<i32> {
    let blank_id = 0i32;
    let mut prev: Option<i32> = None;
    let mut out = Vec::new();
    for t in 0..logits.len_of(Axis(0)) {
        let row = logits.index_axis(Axis(0), t);
        let mut maxv = f32::MIN;
        let mut arg = 0i32;
        for (i, v) in row.iter().enumerate() {
            if *v > maxv {
                maxv = *v;
                arg = i as i32;
            }
        }
        if Some(arg) != prev {
            if arg != blank_id {
                out.push(arg);
            }
            prev = Some(arg);
        }
    }
    out
}
