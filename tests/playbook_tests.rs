use active_call::{
    playbook::{LlmConfig, Playbook, PlaybookConfig},
    synthesis::SynthesisType,
    transcription::TranscriptionType,
};
use dotenvy::dotenv;
use reqwest::Client;
use serde_json::json;
use std::fs;

#[tokio::test]
async fn test_parse_playbook() {
    let content = r#"---
asr:
  provider: "aliyun"
llm:
  provider: "aliyun"
  model: "qwen-plus"
  apiKey: "test-key"
tts:
  provider: "aliyun"
vad:
  provider: "silero"
denoise: true
extra:
  key1: value1
---
Hello, I am an AI assistant.
"#;
    let path = "test_playbook.md";
    fs::write(path, content).unwrap();

    let playbook = Playbook::load(path).await.unwrap();
    assert_eq!(
        playbook
            .config
            .llm
            .as_ref()
            .unwrap()
            .model
            .as_ref()
            .unwrap(),
        "qwen-plus"
    );
    assert_eq!(
        playbook
            .config
            .llm
            .as_ref()
            .unwrap()
            .api_key
            .as_ref()
            .unwrap(),
        "test-key"
    );
    assert_eq!(playbook.config.denoise, Some(true));
    assert_eq!(
        playbook.config.extra.as_ref().unwrap().get("key1").unwrap(),
        "value1"
    );
    assert_eq!(
        playbook
            .config
            .llm
            .as_ref()
            .unwrap()
            .prompt
            .as_ref()
            .unwrap(),
        "Hello, I am an AI assistant."
    );

    fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn test_ai_agent_1002_playbook_has_aliyun_voice_stack_and_greeting() {
    let playbook = Playbook::load("config/playbook/ai_agent_1002.md")
        .await
        .unwrap();

    assert_eq!(
        playbook.config.asr.as_ref().and_then(|asr| asr.provider.clone()),
        Some(TranscriptionType::Aliyun)
    );
    assert_eq!(
        playbook.config.tts.as_ref().and_then(|tts| tts.provider.clone()),
        Some(SynthesisType::Aliyun)
    );
    assert!(playbook.config.llm.is_some(), "ai_agent_1002.md should configure LLM");
    assert_eq!(
        playbook.config.greeting.as_deref(),
        Some("Hello, this is IVR 1002. 你好，这里是 IVR 1002，请问有什么可以帮您？")
    );
}

#[tokio::test]
async fn test_original_style_deepgram_asr_tts_config_loads() {
    let content = r#"---
asr:
  provider: "deepgram"
  language: "en"
  model: "nova-3"
  apiKey: "dg-test-key"
tts:
  provider: "deepgram"
  voice: "aura-2-thalia-en"
  apiKey: "dg-test-key"
llm:
  provider: "openai"
  model: "qwen-plus"
  apiKey: "dashscope-test-key"
  baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1"
---
Test prompt
"#;

    let playbook = Playbook::parse(content).unwrap();

    let asr = playbook.config.asr.as_ref().unwrap();
    assert_eq!(asr.provider, Some(TranscriptionType::Deepgram));
    assert_eq!(asr.model_type.as_deref(), Some("nova-3"));
    assert_eq!(asr.secret_key.as_deref(), Some("dg-test-key"));

    let tts = playbook.config.tts.as_ref().unwrap();
    assert_eq!(tts.provider, Some(SynthesisType::Deepgram));
    assert_eq!(tts.model.as_deref(), Some("aura-2-thalia-en"));
    assert_eq!(tts.secret_key.as_deref(), Some("dg-test-key"));

    let llm = playbook.config.llm.as_ref().unwrap();
    assert_eq!(llm.model.as_deref(), Some("qwen-plus"));
    assert_eq!(llm.api_key.as_deref(), Some("dashscope-test-key"));
}

#[tokio::test]
async fn test_aliyun_llm_integration() {
    dotenv().ok();
    let api_key = std::env::var("ALIYUN_API_KEY").unwrap_or_default();
    if api_key.is_empty() || api_key == "your_aliyun_api_key_here" {
        println!("Skipping Aliyun LLM integration test: ALIYUN_API_KEY not set");
        return;
    }

    let config = PlaybookConfig {
        llm: Some(LlmConfig {
            provider: "aliyun".to_string(),
            model: Some("qwen-plus".to_string()),
            base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".to_string()),
            api_key: Some(api_key),
            prompt: Some("You are a helpful assistant. Reply with 'PONG'.".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let playbook = Playbook {
        config,
        scenes: Default::default(),
        initial_scene_id: None,
        raw_content: String::new(),
    };

    let mut history = Vec::new();
    history.push(json!({
        "role": "system",
        "content": playbook
            .config
            .llm
            .as_ref()
            .unwrap()
            .prompt
            .as_ref()
            .unwrap()
    }));
    history.push(json!({
        "role": "user",
        "content": "PING",
    }));

    let client = Client::new();
    let llm_config = playbook.config.llm.as_ref().unwrap();
    let url = format!("{}/chat/completions", llm_config.base_url.as_ref().unwrap());

    let body = json!({
        "model": llm_config.model.as_ref().unwrap(),
        "messages": history,
    });

    let res = client
        .post(&url)
        .header(
            "Authorization",
            format!("Bearer {}", llm_config.api_key.as_ref().unwrap()),
        )
        .json(&body)
        .send()
        .await;

    match res {
        Ok(response) => {
            assert!(
                response.status().is_success(),
                "Aliyun LLM request failed with status: {}",
                response.status()
            );
            let json: serde_json::Value = response.json().await.unwrap();
            let content = json["choices"][0]["message"]["content"].as_str().unwrap();
            println!("Aliyun LLM response: {}", content);
            assert!(content.contains("PONG"));
        }
        Err(e) => {
            panic!("Failed to connect to Aliyun LLM: {}", e);
        }
    }
}

#[tokio::test]
async fn test_parse_playbook_with_recorder() {
    let content = r#"---
recorder:
  recorderFile: "records/{id}.wav"
  samplerate: 16000
---
Hello
"#;
    let path = "test_playbook_recorder.md";
    fs::write(path, content).unwrap();

    let playbook = Playbook::load(path).await.unwrap();
    assert_eq!(
        playbook.config.recorder.as_ref().unwrap().recorder_file,
        "records/{id}.wav"
    );
    assert_eq!(playbook.config.recorder.as_ref().unwrap().samplerate, 16000);

    fs::remove_file(path).unwrap();
}
