//! Process-wide cache for the ringback classifier weights.
//!
//! `TelcoClassifier` borrows its weights as `&'static [u8]`, so the bytes have
//! to outlive every processor. The previous code read the weights file and
//! `Box::leak`ed it **once per call**, which leaked ~1.4 MB of RSS per call
//! (and re-read the file on every call setup, on the async media path).
//!
//! The cache leaks each distinct weights path exactly once and hands out
//! `Arc<RingbackModel>` handles afterwards, so memory is bounded by the number
//! of distinct configured paths (normally one).

use super::model::TelcoClassifier;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::debug;

/// Weights shared by every inference lane and every call.
pub(crate) struct RingbackModel {
    bytes: &'static [u8],
}

impl RingbackModel {
    /// Stable identity of the weights blob, used as the per-lane runtime key.
    pub(crate) fn id(&self) -> usize {
        self.bytes.as_ptr() as usize
    }

    pub(crate) fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Builds a classifier borrowing the shared weights.
    ///
    /// The classifier must not be shared across threads: on aarch64 it holds a
    /// `RefCell` of Accelerate/NN prepared weights. Each lane builds its own.
    pub(crate) fn classifier(&self) -> TelcoClassifier<'static> {
        TelcoClassifier::new(self.bytes)
    }
}

type Registry = Mutex<HashMap<PathBuf, Arc<RingbackModel>>>;

fn registry() -> &'static Registry {
    static MODELS: OnceLock<Registry> = OnceLock::new();
    MODELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Arc<RingbackModel>>> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Loads (or returns the already loaded) weights for `path`.
pub(crate) fn load(path: impl AsRef<Path>) -> Result<Arc<RingbackModel>> {
    let path = path.as_ref();
    // Canonicalize so `./weights.bin` and `/abs/weights.bin` share one entry.
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    if let Some(model) = lock_registry().get(&key) {
        return Ok(model.clone());
    }

    let data = std::fs::read(&key).with_context(|| {
        format!(
            "Failed to load ringback model weights from {}",
            key.display()
        )
    })?;

    let mut models = lock_registry();
    // Another thread may have loaded the same path while we were reading.
    if let Some(model) = models.get(&key) {
        return Ok(model.clone());
    }

    let bytes: &'static [u8] = Box::leak(data.into_boxed_slice());
    let model = Arc::new(RingbackModel { bytes });
    debug!(path = %key.display(), size = model.size(), "ringback model weights loaded");
    models.insert(key, model.clone());
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights_path() -> Option<PathBuf> {
        crate::media::ringback_detection::test_support::weights_path()
    }

    #[test]
    fn load_is_cached_per_path() {
        let Some(path) = weights_path() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };

        let a = load(&path).unwrap();
        let b = load(&path).unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "repeated loads must reuse the same weights blob"
        );
        assert_eq!(a.id(), b.id());
        assert!(a.size() > 1024 * 1024, "weights look truncated");
    }

    #[test]
    fn load_missing_file_reports_path() {
        let err = load("./definitely-not-a-weights-file.bin")
            .err()
            .expect("missing weights must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("definitely-not-a-weights-file.bin"), "{msg}");
    }

    #[test]
    fn repeated_loads_never_leak_a_second_copy() {
        let Some(path) = weights_path() else {
            eprintln!("skipping: telcoclassifier_weights.bin not found");
            return;
        };
        // 32 call set-ups must all share one leaked blob: a per-call
        // `Box::leak` (the old behaviour) would produce 32 distinct Arcs.
        let first = load(&path).unwrap();
        for i in 0..32 {
            let again = load(&path).unwrap();
            assert!(
                Arc::ptr_eq(&first, &again),
                "load #{i} produced a new weights copy"
            );
        }
    }
}
