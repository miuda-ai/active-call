#[allow(non_snake_case)]
#[allow(unused_variables)]
#[allow(dead_code)]
#[allow(unused_parens)]
#[allow(unused_mut)]
#[allow(unused_imports)]
#[allow(clippy::too_many_arguments)]
mod telcoclassifier {
    include!("telcoclassifier_body.rs");
}
pub use telcoclassifier::TelcoClassifier;
