mod encoder;
mod frontend;
mod tokenizer;

pub use encoder::SensevoiceEncoder;
pub use frontend::{FeaturePipeline, FrontendConfig};
pub use tokenizer::TokenDecoder;

/// Language code to ID mapping for SenseVoice
pub fn language_id_from_code(code: &str) -> i32 {
    // Python mapping: {"auto":0,"zh":3,"en":4,"yue":7,"ja":11,"ko":12,"nospeech":13}
    match code.to_lowercase().as_str() {
        "auto" => 0,
        "zh" => 3,
        "en" => 4,
        "yue" => 7,
        "ja" => 11,
        "ko" => 12,
        "nospeech" => 13,
        _ => 0,
    }
}
