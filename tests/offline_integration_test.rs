use active_call::offline::sensevoice::{FeaturePipeline, FrontendConfig, language_id_from_code};
use active_call::offline::{
    OfflineModels,
    config::OfflineConfig,
    downloader::{ModelDownloader, ModelType},
};
// use std::env;
use std::path::PathBuf;
use tracing_subscriber;

// Ensure we have a place to put models
const TEST_MODEL_DIR: &str = "./target/test_models";

#[cfg(feature = "offline")]
#[tokio::test]
#[ignore] // Start manually
async fn test_offline_integration() -> anyhow::Result<()> {
    // Initialize logging
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let model_dir = PathBuf::from(TEST_MODEL_DIR);
    let downloader = ModelDownloader::new()?;
    // 1. Setup Environment
    unsafe {
        std::env::set_var("HF_ENDPOINT", "https://hf-mirror.com");
    }
    println!("Downloading models to {:?}", model_dir);
    // This might take a while, but it skips existing files
    match downloader.download(ModelType::Sensevoice, &model_dir) {
        Ok(_) => println!("✓ SenseVoice model downloaded"),
        Err(e) => {
            println!("SenseVoice model download error: {}", e);
            return Ok(());
        }
    };
    match downloader.download(ModelType::Supertonic, &model_dir) {
        Ok(_) => println!("✓ Supertonic model downloaded"),
        Err(e) => {
            println!("Supertonic model download error: {}", e);
            return Ok(());
        }
    };

    // 3. Initialize OfflineModels
    // Use fewer threads for test to avoid hogging
    let config = OfflineConfig::new(model_dir.clone(), 2);
    let models = OfflineModels::new(config);

    // Init both
    println!("Initializing models...");
    models.init_supertonic().await?;
    models.init_sensevoice().await?;

    // 4. TTS Synthesis
    let text = "Hello active call";
    println!("Generating system audio for: '{}'", text);

    let tts_lock = models.get_supertonic().await?;
    let mut tts_guard = tts_lock.write().await;
    let tts = tts_guard.as_mut().expect("Supertonic not initialized");

    let mut audio_samples = tts.synthesize(text, "en", None, None)?;
    let sample_rate = tts.sample_rate();
    println!(
        "Generated {} samples from Supertonic at {}Hz",
        audio_samples.len(),
        sample_rate
    );

    if sample_rate != 16000 {
        println!("Resampling to 16000Hz...");
        // Convert f32 to i16 for audio_codec::Resampler
        let i16_samples: Vec<i16> = audio_samples
            .iter()
            .map(|&x| (x * 32767.0).clamp(-32768.0, 32767.0) as i16)
            .collect();

        let mut resampler =
            audio_codec::BoxedResampler::new(sample_rate as usize, 16000).unwrap();
        let resampled_i16 = resampler.resample(&i16_samples);

        // Convert back to f32
        audio_samples = resampled_i16.iter().map(|&x| x as f32 / 32767.0).collect();
    }

    println!(
        "Loaded {} samples from generated audio",
        audio_samples.len()
    );

    // 5. ASR Recognition
    println!("Getting ASR model...");
    let asr_lock = models.get_sensevoice().await?;
    let mut asr_guard = asr_lock.write().await;
    let asr = asr_guard.as_mut().unwrap();

    let mut frontend = FeaturePipeline::new(FrontendConfig::default());
    let feats = frontend.compute_features(&audio_samples, 16000)?;
    // Insert batch dim
    let feats = feats.insert_axis(ndarray::Axis(0));

    let language_code = language_id_from_code("en");

    println!("Inferencing ASR...");
    println!("Running ASR inference...");
    let recognized_text = asr.run_and_decode(feats.view(), language_code, true)?;

    println!("Recognized: '{}'", recognized_text);

    // Verify content
    let norm = recognized_text
        .to_lowercase()
        .replace("-", " ")
        .replace(".", "")
        .replace(",", "");

    assert!(norm.contains("hello"), "ASR failed to match 'hello'");
    assert!(
        norm.contains("active") || norm.contains("call"),
        "ASR failed to match 'active call'"
    );

    Ok(())
}
