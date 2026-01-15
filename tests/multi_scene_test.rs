use active_call::playbook::{
    ChatMessage, DialogueHandler, LlmConfig, Playbook, Scene,
    handler::{LlmHandler, LlmProvider, RagRetriever},
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

struct MockLlmProvider {
    responses: Vec<String>,
    current: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn call(&self, _config: &LlmConfig, _history: &[ChatMessage]) -> Result<String> {
        let idx = self
            .current
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.responses.get(idx).cloned().unwrap_or_default())
    }

    async fn call_stream(
        &self,
        _config: &LlmConfig,
        _history: &[ChatMessage],
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>> {
        let idx = self
            .current
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = self.responses.get(idx).cloned().unwrap_or_default();
        let s = async_stream::stream! {
            yield Ok(response);
        };
        Ok(Box::pin(s))
    }
}

struct NoopRag;
#[async_trait]
impl RagRetriever for NoopRag {
    async fn retrieve(&self, _query: &str) -> Result<String> {
        Ok("".to_string())
    }
}

#[tokio::test]
async fn test_multi_scene_parsing() {
    let content = r#"---
llm:
  provider: "openai"
---
# Scene: intro
Welcome to intro.
# Scene: detail
This is the detail part.
"#;
    let path = "test_multi_scene.md";
    std::fs::write(path, content).unwrap();

    let playbook = Playbook::load(path, None).await.unwrap();
    assert_eq!(playbook.scenes.len(), 2);
    assert!(playbook.scenes.contains_key("intro"));
    assert!(playbook.scenes.contains_key("detail"));

    assert_eq!(
        playbook.scenes.get("intro").unwrap().prompt,
        "Welcome to intro."
    );
    assert_eq!(
        playbook.scenes.get("detail").unwrap().prompt,
        "This is the detail part."
    );

    // Initial prompt should be from the first scene (intro)
    assert_eq!(
        playbook
            .config
            .llm
            .as_ref()
            .unwrap()
            .prompt
            .as_ref()
            .unwrap(),
        "Welcome to intro."
    );

    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn test_scene_transition_logic() -> Result<()> {
    let mut scenes = HashMap::new();
    scenes.insert(
        "intro".to_string(),
        Scene {
            id: "intro".to_string(),
            prompt: "You are in intro.".to_string(),
            dtmf: None,
            play: None,
        },
    );
    scenes.insert(
        "detail".to_string(),
        Scene {
            id: "detail".to_string(),
            prompt: "You are in detail.".to_string(),
            dtmf: None,
            play: None,
        },
    );

    let llm_config = LlmConfig {
        provider: "mock".to_string(),
        prompt: Some("You are in intro.".to_string()),
        ..Default::default()
    };

    // First response triggers goto, second response is in new scene
    let provider = Arc::new(MockLlmProvider {
        responses: vec![
            "Going to detail now. <goto scene=\"detail\" />".to_string(),
            "I am now in detail.".to_string(),
        ],
        current: std::sync::atomic::AtomicUsize::new(0),
    });

    let mut handler = LlmHandler::with_provider(
        llm_config,
        provider,
        Arc::new(NoopRag),
        Default::default(),
        scenes,
        None,
        None,
    );

    // 1. Initial State
    assert_eq!(
        handler.get_history_ref()[0]
            .content
            .contains("You are in intro."),
        true
    );

    // 2. Trigger Transition
    // Note: on_event(AsrFinal) usually triggers generate_response
    // We can simulate this by calling on_event or directly testing the internal buffer logic if exposed,
    // but LlmHandler::on_event is the public API.

    use active_call::event::SessionEvent;
    let event = SessionEvent::AsrFinal {
        track_id: "test-track".to_string(),
        timestamp: 1000,
        index: 1,
        start_time: Some(0),
        end_time: Some(1000),
        text: "Tell me more".to_string(),
        is_filler: Some(false),
        confidence: Some(1.0),
    };

    let commands = handler.on_event(&event).await?;

    // Check if transition happened
    assert!(
        handler
            .get_current_scene_id()
            .as_ref()
            .map(|s| s == "detail")
            .unwrap_or(false)
    );

    // Check if history was updated with new system prompt
    assert_eq!(
        handler.get_history_ref()[0]
            .content
            .contains("You are in detail."),
        true
    );

    // The command should still contain the text before the tag
    // LlmHandler creates Play/Tts commands.
    // In our mock, it yielded "Going to detail now. <goto scene=\"detail\" />"
    // The extract_streaming_commands should have pushed a TTS command for the prefix.

    let tts_cmd_exists = commands.iter().any(|c| match c {
        active_call::call::Command::Tts { text, .. } => text.contains("Going to detail now"),
        _ => false,
    });
    assert!(tts_cmd_exists);

    Ok(())
}
