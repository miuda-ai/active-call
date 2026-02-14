use super::*;
use crate::event::SessionEvent;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;

struct TestProvider {
    responses: Mutex<VecDeque<String>>,
}

impl TestProvider {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

#[async_trait]
impl LlmProvider for TestProvider {
    async fn call(&self, _config: &LlmConfig, _history: &[ChatMessage]) -> Result<String> {
        let mut guard = self.responses.lock().unwrap();
        guard
            .pop_front()
            .ok_or_else(|| anyhow!("Test provider ran out of responses"))
    }

    async fn call_stream(
        &self,
        _config: &LlmConfig,
        _history: &[ChatMessage],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LlmStreamEvent>> + Send>>> {
        let response = self.call(_config, _history).await?;
        let s = async_stream::stream! {
            yield Ok(LlmStreamEvent::Content(response));
        };
        Ok(Box::pin(s))
    }
}

struct RecordingRag {
    queries: Mutex<Vec<String>>,
}

impl RecordingRag {
    fn new() -> Self {
        Self {
            queries: Mutex::new(Vec::new()),
        }
    }

    fn recorded_queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}

#[async_trait]
impl RagRetriever for RecordingRag {
    async fn retrieve(&self, query: &str) -> Result<String> {
        self.queries.lock().unwrap().push(query.to_string());
        Ok(format!("retrieved {}", query))
    }
}

#[test]
fn test_build_system_prompt_with_features() {
    let config = LlmConfig {
        prompt: Some("Base prompt".to_string()),
        language: Some("zh".to_string()),
        features: Some(vec!["intent_clarification".to_string()]),
        ..Default::default()
    };

    let prompt = LlmHandler::build_system_prompt(&config, None, None);
    assert!(prompt.contains("Base prompt"));
    assert!(prompt.contains("### Enhanced Capabilities:"));
    // This is the content of intent_clarification.zh.md
    assert!(prompt.contains("如果用户意图模糊"));
    assert!(prompt.contains("<hangup/>"));
}

#[test]
fn test_build_system_prompt_missing_feature() {
    let config = LlmConfig {
        prompt: Some("Base prompt".to_string()),
        language: Some("zh".to_string()),
        features: Some(vec!["non_existent_feature".to_string()]),
        ..Default::default()
    };

    // Should not crash, just warn and omit the feature
    let prompt = LlmHandler::build_system_prompt(&config, None, None);
    assert!(prompt.contains("Base prompt"));
    assert!(!prompt.contains("Enhanced Capabilities"));
}

#[test]
fn test_build_system_prompt_en() {
    let config = LlmConfig {
        prompt: Some("Base prompt".to_string()),
        language: Some("en".to_string()),
        features: Some(vec!["intent_clarification".to_string()]),
        ..Default::default()
    };

    let prompt = LlmHandler::build_system_prompt(&config, None, None);
    assert!(prompt.contains("If the user's intent is unclear"));
}

#[tokio::test]
async fn handler_applies_tool_instructions() -> Result<()> {
    let response = r#"{
        "text": "Goodbye",
        "waitInputTimeout": 15000,
        "tools": [
            {"name": "hangup", "reason": "done", "initiator": "agent"},
            {"name": "refer", "caller": "sip:bot", "callee": "sip:lead"}
        ]
    }"#;

    let provider = Arc::new(TestProvider::new(vec![response.to_string()]));
    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    let event = SessionEvent::AsrFinal {
        track_id: "track-1".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "hello".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };

    let commands = handler.on_event(&event).await?;
    assert!(matches!(
        commands.get(0),
        Some(Command::Tts {
            text,
            wait_input_timeout: Some(15000),
            auto_hangup: Some(true),
            ..
        }) if text == "Goodbye"
    ));
    assert!(commands.iter().any(|cmd| matches!(
        cmd,
        Command::Refer {
            caller,
            callee,
            ..
        } if caller == "sip:bot" && callee == "sip:lead"
    )));

    Ok(())
}

#[tokio::test]
async fn handler_requeries_after_rag() -> Result<()> {
    let rag_instruction = r#"{"tools": [{"name": "rag", "query": "policy"}]}"#;
    let provider = Arc::new(TestProvider::new(vec![
        rag_instruction.to_string(),
        "Final answer".to_string(),
    ]));
    let rag = Arc::new(RecordingRag::new());
    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        rag.clone(),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    let event = SessionEvent::AsrFinal {
        track_id: "track-2".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "reep".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };

    let commands = handler.on_event(&event).await?;
    assert!(matches!(
        commands.get(0),
        Some(Command::Tts {
            text,
            wait_input_timeout: Some(timeout),
            ..
        }) if text == "Final answer" && *timeout == 10000
    ));
    assert_eq!(rag.recorded_queries(), vec!["policy".to_string()]);

    Ok(())
}

#[tokio::test]
async fn test_full_dialogue_flow() -> Result<()> {
    let responses = vec![
        "Hello! How can I help you today?".to_string(),
        r#"{"text": "I can help with that. Anything else?", "waitInputTimeout": 5000}"#.to_string(),
        r#"{"text": "Goodbye!", "tools": [{"name": "hangup", "reason": "completed"}]}"#.to_string(),
    ];

    let provider = Arc::new(TestProvider::new(responses));
    let config = LlmConfig {
        greeting: Some("Welcome to the voice assistant.".to_string()),
        ..Default::default()
    };

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    // 1. Start the dialogue
    let commands = handler.on_start().await?;
    assert_eq!(commands.len(), 1);
    if let Command::Tts { text, .. } = &commands[0] {
        assert_eq!(text, "Welcome to the voice assistant.");
    } else {
        panic!("Expected Tts command");
    }

    // 2. User says something
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "I need help".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    let commands = handler.on_event(&event).await?;
    // "Hello! How can I help you today?" -> split into two + EOS
    assert_eq!(commands.len(), 3);
    if let Command::Tts { text, .. } = &commands[0] {
        assert!(text.contains("Hello"));
    } else {
        panic!("Expected Tts command");
    }

    // 3. User says something else
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 1,
        start_time: None,
        end_time: None,
        text: "Tell me a joke".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    let commands = handler.on_event(&event).await?;
    assert_eq!(commands.len(), 1);
    if let Command::Tts {
        text,
        wait_input_timeout,
        ..
    } = &commands[0]
    {
        assert_eq!(text, "I can help with that. Anything else?");
        assert_eq!(*wait_input_timeout, Some(5000));
    } else {
        panic!("Expected Tts command");
    }

    // 4. User says goodbye
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 2,
        start_time: None,
        end_time: None,
        text: "That's all, thanks".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    let commands = handler.on_event(&event).await?;
    // Should have Tts with auto_hangup
    assert_eq!(commands.len(), 1);

    let has_tts_hangup = commands.iter().any(|c| {
        matches!(
            c,
            Command::Tts {
                text,
                auto_hangup: Some(true),
                ..
            } if text == "Goodbye!"
        )
    });

    assert!(has_tts_hangup);

    Ok(())
}

#[tokio::test]
async fn test_xml_tools_and_sentence_splitting() -> Result<()> {
    let responses = vec!["Hello! <refer to=\"sip:123\"/> How are you? <hangup/>".to_string()];
    let provider = Arc::new(TestProvider::new(responses));
    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "hi".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };

    let commands = handler.on_event(&event).await?;

    // Expected commands:
    // 1. TTS "Hello! "
    // 2. Refer "sip:123"
    // 3. TTS " How are you? "
    // 4. Empty TTS with auto_hangup (no separate Hangup command anymore)
    assert_eq!(commands.len(), 4);

    if let Command::Tts {
        text,
        play_id: pid1,
        ..
    } = &commands[0]
    {
        assert!(text.contains("Hello"));
        assert!(pid1.is_some());

        if let Command::Refer { callee, .. } = &commands[1] {
            assert_eq!(callee, "sip:123");
        } else {
            panic!("Expected Refer");
        }

        if let Command::Tts {
            text,
            play_id: pid2,
            ..
        } = &commands[2]
        {
            assert!(text.contains("How are you"));
            assert_eq!(*pid1, *pid2); // Same play_id
        } else {
            panic!("Expected Tts");
        }

        if let Command::Tts {
            auto_hangup: Some(true),
            end_of_stream: Some(true),
            ..
        } = &commands[3]
        {
            // Good - auto_hangup TTS instead of separate Hangup command
        } else {
            panic!("Expected Tts with auto_hangup and end_of_stream");
        }
    } else {
        panic!("Expected Tts");
    }

    Ok(())
}

#[tokio::test]
async fn test_interruption_logic() -> Result<()> {
    let provider = Arc::new(TestProvider::new(vec!["Some long response".to_string()]));
    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    // 1. Trigger a response
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "hello".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    handler.on_event(&event).await?;
    assert!(handler.is_speaking);

    // Sleep to bypass the 800ms interruption guard
    tokio::time::sleep(std::time::Duration::from_millis(850)).await;

    // 2. Simulate user starting to speak (AsrDelta)
    let event = SessionEvent::AsrDelta {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "I...".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    let commands = handler.on_event(&event).await?;
    assert_eq!(commands.len(), 1);
    assert!(matches!(commands[0], Command::Interrupt { .. }));
    assert!(!handler.is_speaking);

    Ok(())
}

#[tokio::test]
async fn test_rag_iteration_limit() -> Result<()> {
    // Provider that always returns a RAG tool call
    let rag_instruction = r#"{"tools": [{"name": "rag", "query": "endless"}]}"#;
    let provider = Arc::new(TestProvider::new(vec![
        rag_instruction.to_string(),
        rag_instruction.to_string(),
        rag_instruction.to_string(),
        rag_instruction.to_string(),
        "Should not reach here".to_string(),
    ]));

    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(RecordingRag::new()),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "loop".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };

    let commands = handler.on_event(&event).await?;
    // After 3 attempts (MAX_RAG_ATTEMPTS), it should stop and return the last raw response
    assert_eq!(commands.len(), 1);
    if let Command::Tts { text, .. } = &commands[0] {
        assert_eq!(text, rag_instruction);
    }

    Ok(())
}

#[tokio::test]
async fn test_follow_up_logic() -> Result<()> {
    use std::time::Duration;

    // 1. Setup handler with follow-up config
    let follow_up_config = super::super::FollowUpConfig {
        timeout: 100, // 100ms for testing
        max_count: 2,
    };

    // Provider that returns responses for follow-ups
    let provider = Arc::new(TestProvider::new(vec![
        "Follow up 1".to_string(),
        "Follow up 2".to_string(),
        "Response to user".to_string(),
    ]));

    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        Some(follow_up_config),
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    // 2. Simulate initial interaction end
    handler.last_interaction_at = std::time::Instant::now();
    handler.is_speaking = false;

    // 3. Silence < timeout
    let event = SessionEvent::Silence {
        track_id: "t1".to_string(),
        timestamp: 0,
        start_time: 0,
        duration: 50,
        samples: None,
    };
    let commands = handler.on_event(&event).await?;
    assert!(commands.is_empty(), "Should not trigger if < timeout");

    // 4. Silence >= timeout
    tokio::time::sleep(Duration::from_millis(110)).await;
    // We need to act as if time passed. logic uses last_interaction_at.elapsed()
    // Wait, handler.last_interaction_at was set to now(). Sleep ensures elapsed() > 100ms.

    let event = SessionEvent::Silence {
        track_id: "t1".to_string(),
        timestamp: 0,
        start_time: 0,
        duration: 100,
        samples: None,
    };
    let commands = handler.on_event(&event).await?;
    assert_eq!(commands.len(), 1, "Should trigger follow-up 1");
    if let Command::Tts { text, .. } = &commands[0] {
        assert_eq!(text, "Follow up 1");
    }
    assert_eq!(handler.consecutive_follow_ups, 1);

    // 4b. Simulate bot finishing speaking Follow up 1
    // generate_response sets is_speaking = true. We need to clear it.
    let event = SessionEvent::TrackEnd {
        track_id: "t1".to_string(),
        timestamp: 0,
        play_id: None,
        duration: 100,
        ssrc: 0,
    };
    handler.on_event(&event).await?;
    assert!(
        !handler.is_speaking,
        "Bot should not be speaking after TrackEnd"
    );

    // Wait again for timeout
    tokio::time::sleep(Duration::from_millis(110)).await;
    let event = SessionEvent::Silence {
        track_id: "t1".to_string(),
        timestamp: 0,
        start_time: 0,
        duration: 100,
        samples: None,
    };
    let commands = handler.on_event(&event).await?;
    assert_eq!(commands.len(), 1, "Should trigger follow-up 2");
    if let Command::Tts { text, .. } = &commands[0] {
        assert_eq!(text, "Follow up 2");
    }
    assert_eq!(handler.consecutive_follow_ups, 2);

    // 5b. Simulate bot finishing speaking Follow up 2
    let event = SessionEvent::TrackEnd {
        track_id: "t1".to_string(),
        timestamp: 0,
        play_id: None,
        duration: 100,
        ssrc: 0,
    };
    handler.on_event(&event).await?;

    // 6. Max count reached
    tokio::time::sleep(Duration::from_millis(110)).await;
    let event = SessionEvent::Silence {
        track_id: "t1".to_string(),
        timestamp: 0,
        start_time: 0,
        duration: 100,
        samples: None,
    };
    let commands = handler.on_event(&event).await?;
    assert_eq!(commands.len(), 1, "Should hangup after max count");
    assert!(matches!(commands[0], Command::Hangup { .. }));

    handler.consecutive_follow_ups = 2; // Artificially set high
    let event = SessionEvent::AsrFinal {
        track_id: "t1".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "User speaks".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };

    let _ = handler.on_event(&event).await?;
    assert_eq!(
        handler.consecutive_follow_ups, 0,
        "Should reset count on AsrFinal"
    );

    Ok(())
}

#[tokio::test]
async fn test_interruption_protection_period() -> Result<()> {
    let provider = Arc::new(TestProvider::new(vec!["Some long response".to_string()]));
    let mut config = crate::playbook::InterruptionConfig::default();
    config.ignore_first_ms = Some(800);

    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        config,
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    // 1. Trigger a response
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "hello".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    handler.on_event(&event).await?;
    assert!(handler.is_speaking);

    // 2. Simulate user starting to speak immediately (AsrDelta)
    let event = SessionEvent::AsrDelta {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "I...".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    let commands = handler.on_event(&event).await?;
    // Should be ignored due to protection period
    assert_eq!(commands.len(), 0);
    assert!(handler.is_speaking);

    Ok(())
}

#[tokio::test]
async fn test_interruption_filler_word() -> Result<()> {
    let provider = Arc::new(TestProvider::new(vec!["Some long response".to_string()]));
    let mut config = crate::playbook::InterruptionConfig::default();
    config.filler_word_filter = Some(true);
    config.ignore_first_ms = Some(0); // Disable protection period for this test

    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        config,
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    // 1. Trigger a response
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "hello".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };
    handler.on_event(&event).await?;
    assert!(handler.is_speaking);

    // Sleep to bypass the 500ms stale event guard
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    // 2. Simulate user saying "uh" (filler)
    let event = SessionEvent::AsrDelta {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "uh".to_string(),
        is_filler: Some(true),
        confidence: None,
        task_id: None,
    };
    let commands = handler.on_event(&event).await?;
    // Should be ignored
    assert_eq!(commands.len(), 0);
    assert!(handler.is_speaking);

    // 3. Simulate user saying "Wait" (not filler)
    let event = SessionEvent::AsrDelta {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "Wait".to_string(),
        is_filler: Some(false),
        confidence: None,
        task_id: None,
    };
    let commands = handler.on_event(&event).await?;
    // Should trigger interruption
    assert_eq!(commands.len(), 1);
    assert!(matches!(commands[0], Command::Interrupt { .. }));

    Ok(())
}

#[tokio::test]
async fn test_eou_early_response() -> Result<()> {
    let provider = Arc::new(TestProvider::new(vec![
        "End of Utterance response".to_string(),
    ]));
    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    // 1. Receive EOU
    let event = SessionEvent::Eou {
        track_id: "test".to_string(),
        timestamp: 0,
        completed: true,
        interrupt_point: None,
    };
    let commands = handler.on_event(&event).await?;
    assert_eq!(commands.len(), 1);
    if let Command::Tts { text, .. } = &commands[0] {
        assert_eq!(text, "End of Utterance response");
    } else {
        panic!("Expected Tts");
    }

    Ok(())
}

#[tokio::test]
async fn test_summary_and_history() -> Result<()> {
    let provider = Arc::new(TestProvider::new(vec!["Test summary".to_string()]));
    let mut handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    // Add some history
    handler.history.push(ChatMessage {
        role: "user".to_string(),
        content: "Hello".to_string(),
    });
    handler.history.push(ChatMessage {
        role: "assistant".to_string(),
        content: "Hi there".to_string(),
    });

    let history = handler.get_history().await;
    assert_eq!(history.len(), 3);

    let summary = handler.summarize("Summarize this").await?;
    assert_eq!(summary, "Test summary");

    Ok(())
}

#[tokio::test]
async fn test_rolling_summary() -> Result<()> {
    let responses = vec![
        "This is the summary of previous conversation.".to_string(),
        "Response to user input after summary.".to_string(),
    ];
    let provider = Arc::new(TestProvider::new(responses));

    let mut config = LlmConfig::default();
    config.features = Some(vec!["rolling_summary".to_string()]);
    config.summary_limit = Some(4);

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
    );

    for i in 1..=12 {
        let role = if i % 2 == 1 { "user" } else { "assistant" };
        handler.history.push(ChatMessage {
            role: role.to_string(),
            content: format!("Message {}", i),
        });
    }
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        timestamp: 0,
        index: 0,
        start_time: None,
        end_time: None,
        text: "Trigger summary".to_string(),
        is_filler: None,
        confidence: None,
        task_id: None,
    };

    let commands = handler.on_event(&event).await?;

    if let Command::Tts { text, .. } = &commands[0] {
        assert_eq!(text, "Response to user input after summary.");
    } else {
        panic!("Expected Tts");
    }

    assert_eq!(handler.history.len(), 8);

    // Verify system prompt contains summary
    let system_msg = &handler.history[0];
    assert_eq!(system_msg.role, "system");
    assert!(
        system_msg
            .content
            .contains("[Previous Context Summary]: This is the summary of previous conversation.")
    );

    assert_eq!(
        handler.history.last().unwrap().content,
        "Response to user input after summary."
    );
    assert_eq!(
        handler.history[handler.history.len() - 2].content,
        "Trigger summary"
    );
    assert_eq!(
        handler.history[handler.history.len() - 3].content,
        "Message 12"
    );

    Ok(())
}

#[tokio::test]
async fn test_set_var_extraction() {
    let config = LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        None,
    );

    let mut buffer = "Hello <set_var key=\"foo\" value=\"bar\" /> world".to_string();
    let cmds = handler
        .extract_streaming_commands(&mut buffer, "test_p", false)
        .await;

    // It should have extracted "Hello " as TTS command
    assert_eq!(cmds.len(), 1);
    if let Command::Tts { text, .. } = &cmds[0] {
        assert_eq!(text, "Hello ");
    } else {
        panic!("Expected TTS command");
    }

    // The buffer should now start with " world" (leading space might be kept or consumed depending on regex?)
    // The regex for SetVar consumes the tag.
    // "Hello " was drained. Tag was drained.
    // Buffer should contain " world".
    assert_eq!(buffer, " world");
}

#[tokio::test]
async fn test_multiple_set_vars() {
    let config = LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        None,
    );

    let mut buffer =
        "<set_var key=\"k1\" value=\"v1\" /><set_var key=\"k2\" value=\"v2\" />".to_string();
    // First extraction
    let cmds = handler
        .extract_streaming_commands(&mut buffer, "test_p", false)
        .await;
    assert_eq!(cmds.len(), 0); // No text prefix

    // Should have consumed first tag.
    // Loop in extract_streaming_commands?
    // extract_streaming_commands loop:
    // loop {
    //    find first occurrence
    //    if match, process and drain, then CONTINUE loop?
    // }
    // Let's check extract_streaming_commands loop logic.
}

#[tokio::test]
async fn test_set_var_updates_state() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use tokio_util::sync::CancellationToken;

    let config = crate::playbook::LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    // Setup ActiveCall
    // Use a random port or default config to avoid binding conflicts if any
    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-set-var".to_string();
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        None,
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None, // global_follow_up
        std::collections::HashMap::new(),
        None, // dtmf
        None, // dtmf_collectors
        None, // initial_scene_id
        None, // sip_config
    );
    handler.call = Some(active_call.clone());

    // Initial buffer with set_var
    let mut buffer = "<set_var key=\"my_key\" value=\"my_val\" />".to_string();

    // We called extract_streaming_commands. It should process the tag and update state.
    handler
        .extract_streaming_commands(&mut buffer, "p_id", false)
        .await;

    // Check strict equality of buffer (should be drained) or at least the tag removed
    assert!(
        buffer.is_empty(),
        "Buffer should be empty after processing set_var, got: '{}'",
        buffer
    );

    // Check ActiveCall state
    let state = active_call.call_state.read().await;
    let extras = state
        .extras
        .as_ref()
        .expect("extras should be initialized/set");

    assert_eq!(
        extras.get("my_key").unwrap(),
        &serde_json::Value::String("my_val".to_string()),
        "Variable my_key should be set to my_val"
    );
}

#[tokio::test]
async fn test_set_var_with_sip_headers() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use tokio_util::sync::CancellationToken;

    let config = crate::playbook::LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-sip-headers".to_string();
    let track_config = TrackConfig::default();

    // Initialize with some extracted headers
    let mut initial_extras = std::collections::HashMap::new();
    initial_extras.insert("X-CID".to_string(), serde_json::json!("123456"));

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        Some(initial_extras),
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        None,
    );
    handler.call = Some(active_call.clone());

    // Simulate LLM setting _hangup_headers for BYE
    let mut buffer = r#"<set_var key="_hangup_headers" value='{"X-Hangup-Reason":"completed","X-Duration":"120"}' />"#.to_string();

    handler
        .extract_streaming_commands(&mut buffer, "p_id", true)
        .await;

    assert!(
        buffer.is_empty(),
        "Buffer should be empty, got: '{}'",
        buffer
    );

    let state = active_call.call_state.read().await;
    let extras = state.extras.as_ref().unwrap();

    // Verify original header still exists
    assert_eq!(extras.get("X-CID").unwrap(), &serde_json::json!("123456"));

    // Verify new _hangup_headers was set
    assert!(extras.contains_key("_hangup_headers"));
    let headers_value = extras.get("_hangup_headers").unwrap();
    assert_eq!(
        headers_value,
        &serde_json::Value::String(
            r#"{"X-Hangup-Reason":"completed","X-Duration":"120"}"#.to_string()
        )
    );
}

#[tokio::test]
async fn test_http_command_in_stream() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let config = crate::playbook::LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-http".to_string();
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        None,
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        None,
    );
    handler.call = Some(active_call.clone());

    // Start mock server
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "success",
            "data": "test_value"
        })))
        .mount(&mock_server)
        .await;

    let url = format!("{}/api/data", mock_server.uri());
    let mut buffer = format!(r#"Check this <http url="{}" />"#, url);

    let initial_history_len = handler.history.len();

    handler
        .extract_streaming_commands(&mut buffer, "p_id", true)
        .await;

    // History should have system message with HTTP response
    assert!(
        handler.history.len() > initial_history_len,
        "History should grow after HTTP call"
    );

    let last_msg = handler.history.last().unwrap();
    assert_eq!(last_msg.role, "system");
    assert!(last_msg.content.contains("HTTP GET"));
    assert!(last_msg.content.contains("200"));
    assert!(last_msg.content.contains("test_value"));
}

#[tokio::test]
async fn test_http_command_post_with_body() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{body_string, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let config = crate::playbook::LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-http-post".to_string();
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        None,
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        None,
    );
    handler.call = Some(active_call.clone());

    // Start mock server
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/submit"))
        .and(body_string("test_payload"))
        .respond_with(ResponseTemplate::new(201).set_body_string("Created"))
        .mount(&mock_server)
        .await;

    let url = format!("{}/api/submit", mock_server.uri());
    let mut buffer = format!(
        r#"Submitting <http url="{}" method="POST" body="test_payload" />"#,
        url
    );

    handler
        .extract_streaming_commands(&mut buffer, "p_id", true)
        .await;

    let last_msg = handler.history.last().unwrap();
    assert_eq!(last_msg.role, "system");
    assert!(last_msg.content.contains("HTTP POST"));
    assert!(last_msg.content.contains("201"));
}

#[tokio::test]
async fn test_multiple_commands_in_sequence() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use tokio_util::sync::CancellationToken;

    let config = crate::playbook::LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-multi".to_string();
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        None,
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        None,
    );
    handler.call = Some(active_call.clone());

    // Test combining set_var and normal text
    let mut buffer =
        r#"Hello <set_var key="user_name" value="Alice" /> nice to meet you!"#.to_string();

    let commands = handler
        .extract_streaming_commands(&mut buffer, "p_id", true)
        .await;

    // Should generate TTS commands
    assert!(!commands.is_empty());

    // Check state was updated
    let state = active_call.call_state.read().await;
    let extras = state.extras.as_ref().unwrap();
    assert_eq!(
        extras.get("user_name").unwrap(),
        &serde_json::Value::String("Alice".to_string())
    );
}
#[tokio::test]
async fn test_set_var_individual_sip_header() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use tokio_util::sync::CancellationToken;

    let config = crate::playbook::LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-individual-header".to_string();
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        None,
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        None,
    );
    handler.call = Some(active_call.clone());

    // Set individual SIP headers using set_var
    let mut buffer = r#"<set_var key="X-Call-Status" value="answered" />"#.to_string();
    handler
        .extract_streaming_commands(&mut buffer, "p1", true)
        .await;

    let mut buffer2 = r#"<set_var key="X-Call-Duration" value="120" />"#.to_string();
    handler
        .extract_streaming_commands(&mut buffer2, "p2", true)
        .await;

    let state = active_call.call_state.read().await;
    let extras = state.extras.as_ref().unwrap();

    // Both headers should be set
    assert_eq!(
        extras.get("X-Call-Status").unwrap(),
        &serde_json::json!("answered")
    );
    assert_eq!(
        extras.get("X-Call-Duration").unwrap(),
        &serde_json::json!("120")
    );
}

#[tokio::test]
async fn test_bye_headers_with_all_variables() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use std::collections::HashMap as StdHashMap;
    use tokio_util::sync::CancellationToken;

    let mut sip_config = crate::SipOption::default();
    let mut hangup_headers = StdHashMap::new();
    hangup_headers.insert("X-Call-Result".to_string(), "{{ call_result }}".to_string());
    hangup_headers.insert(
        "X-Customer-ID".to_string(),
        "{{ sip[\"X-CID\"] }}".to_string(),
    );
    hangup_headers.insert("X-Agent-Name".to_string(), "{{ agent_name }}".to_string());
    sip_config.hangup_headers = Some(hangup_headers);

    let llm_config = crate::playbook::LlmConfig::default();
    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-bye-headers".to_string();
    let track_config = TrackConfig::default();

    // Initialize with mixed variables - SIP headers and regular variables
    let mut initial_extras = std::collections::HashMap::new();
    initial_extras.insert("X-CID".to_string(), serde_json::json!("CUSTOMER-123"));
    initial_extras.insert("call_result".to_string(), serde_json::json!("successful"));
    initial_extras.insert("agent_name".to_string(), serde_json::json!("Alice"));
    // Mark X-CID as SIP header
    initial_extras.insert("_sip_header_keys".to_string(), serde_json::json!(["X-CID"]));

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        Some(initial_extras),
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        llm_config,
        provider,
        rag,
        interruption,
        None,
        std::collections::HashMap::new(),
        None,
        None,
        None,
        Some(sip_config),
    );
    handler.call = Some(active_call.clone());

    // Render BYE headers
    let rendered_headers = handler.render_sip_headers().await;

    assert!(rendered_headers.is_some());
    let headers = rendered_headers.unwrap();

    // Verify all variables were accessible
    assert_eq!(headers.get("X-Call-Result").unwrap(), "successful");
    assert_eq!(headers.get("X-Customer-ID").unwrap(), "CUSTOMER-123");
    assert_eq!(headers.get("X-Agent-Name").unwrap(), "Alice");
}

#[tokio::test]
async fn test_bye_headers_with_unset_variables() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use std::collections::HashMap as StdHashMap;
    use tokio_util::sync::CancellationToken;

    // Test case: user defines hangup_headers with variables that are NOT set yet
    // This simulates the user's problem in the screenshot
    let llm_config = crate::playbook::LlmConfig::default();

    let mut sip_config = crate::SipOption::default();
    let mut hangup_headers = StdHashMap::new();
    hangup_headers.insert(
        "X-Hangupreason".to_string(),
        "{{ hangupreason }}".to_string(),
    );
    hangup_headers.insert(
        "X-Skillgroupid".to_string(),
        "{{ skillgroupid }}".to_string(),
    );
    sip_config.hangup_headers = Some(hangup_headers);

    let interruption = crate::playbook::InterruptionConfig::default();
    let provider = Arc::new(TestProvider::new(vec![]));
    let rag = Arc::new(RecordingRag::new());

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-unset-vars".to_string();
    let track_config = TrackConfig::default();

    // Initialize WITHOUT setting hangupreason and skillgroupid
    let initial_extras = StdHashMap::new();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        Some(initial_extras),
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        llm_config,
        provider,
        rag,
        interruption,
        None,
        StdHashMap::new(),
        None,
        None,
        None,
        Some(sip_config),
    );
    handler.call = Some(active_call.clone());

    // Render BYE headers - this should handle missing variables gracefully
    let rendered_headers = handler.render_sip_headers().await;

    assert!(rendered_headers.is_some());
    let headers = rendered_headers.unwrap();

    // When variables are not set, MiniJinja renders empty strings
    println!("Rendered headers when variables not set: {:?}", headers);

    // This is the BUG: MiniJinja renders undefined variables as empty strings!
    assert_eq!(headers.get("X-Hangupreason").unwrap(), "");
    assert_eq!(headers.get("X-Skillgroupid").unwrap(), "");
}

#[tokio::test]
async fn test_set_var_then_bye_headers() {
    // Test the complete flow: set_var in streaming, then render BYE headers
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use std::collections::HashMap as StdHashMap;
    use tokio_util::sync::CancellationToken;

    let llm_config = crate::playbook::LlmConfig {
        provider: "test".to_string(),
        ..Default::default()
    };

    let mut sip_config = crate::SipOption::default();
    let mut hangup_headers = StdHashMap::new();
    hangup_headers.insert(
        "X-Hangupreason".to_string(),
        "{{ hangupreason }}".to_string(),
    );
    hangup_headers.insert(
        "X-Skillgroupid".to_string(),
        "{{ skillgroupid }}".to_string(),
    );
    sip_config.hangup_headers = Some(hangup_headers);

    // LLM will return a response with set_var commands
    let provider = Arc::new(TestProvider::new(vec![
        r#"好的，我帮您转接人工 <set_var key="hangupreason" value="Transfer"/> <set_var key="skillgroupid" value="7084rx000003"/> <hangup/>"#.to_string(),
    ]));

    let rag = Arc::new(RecordingRag::new());
    let interruption = crate::playbook::InterruptionConfig::default();

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-set-var-flow".to_string();
    let track_config = TrackConfig::default();

    let initial_extras = StdHashMap::new();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        Some(initial_extras),
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        llm_config,
        provider,
        rag,
        interruption,
        None,
        StdHashMap::new(),
        None,
        None,
        None,
        Some(sip_config),
    );
    handler.call = Some(active_call.clone());

    // Simulate the dialogue flow
    let commands = handler.generate_response().await.unwrap();

    // Process the streaming response which includes set_var commands
    // The commands should include TTS and eventually trigger set_var processing
    println!("Generated commands: {:?}", commands);

    // Now check if variables were set in state
    let state = active_call.call_state.read().await;
    if let Some(extras) = &state.extras {
        println!("Extras after generate_response: {:?}", extras);

        // Check if set_var worked
        if let Some(reason) = extras.get("hangupreason") {
            assert_eq!(reason.as_str().unwrap(), "Transfer");
        } else {
            println!("WARNING: hangupreason not found in extras!");
        }

        if let Some(skill) = extras.get("skillgroupid") {
            assert_eq!(skill.as_str().unwrap(), "7084rx000003");
        } else {
            println!("WARNING: skillgroupid not found in extras!");
        }
    } else {
        println!("WARNING: extras is None!");
    }
    drop(state);

    // Now render BYE headers
    let rendered_headers = handler.render_sip_headers().await;

    assert!(rendered_headers.is_some());
    let headers = rendered_headers.unwrap();

    println!("Rendered BYE headers: {:?}", headers);

    // The headers should now contain the set values
    assert_eq!(headers.get("X-Hangupreason").unwrap(), "Transfer");
    assert_eq!(headers.get("X-Skillgroupid").unwrap(), "7084rx000003");
}

#[tokio::test]
async fn test_streaming_chunks_with_incomplete_set_var() {
    // Test case: streaming output splits <set_var> tag across multiple chunks
    // This simulates what we see in the user's screenshot

    let content =
        r#"好的，我帮您转接人工 <set_var key="hangupreason" value="Transfer"/> <hangup/>"#;

    // Simulate the tag being split in the middle (incomplete buffer state)
    let incomplete_buffer1 = r#"好的，我帮您转接人工 <set_var key="hangupreason" value="Transfer"#;
    // Test that incomplete tags are NOT matched
    assert!(
        super::RE_SET_VAR.captures(incomplete_buffer1).is_none(),
        "Incomplete set_var should not be matched"
    );

    // Test that complete tags ARE matched
    assert!(
        super::RE_SET_VAR.captures(content).is_some(),
        "Complete set_var should be matched"
    );

    // Test critical scenario: hangup appears before set_var is complete
    let bad_order = r#"好的，我帮您转接 <hangup/> <set_var key="hangupreason" value="Transfer"/>"#;

    let hangup_match = super::RE_HANGUP.find(bad_order);
    let setvar_match = super::RE_SET_VAR.captures(bad_order);

    if let (Some(h), Some(s)) = (hangup_match, setvar_match) {
        println!(
            "Hangup position: {}, SetVar position: {}",
            h.start(),
            s.get(0).unwrap().start()
        );
        assert!(
            h.start() < s.get(0).unwrap().start(),
            "BUG: hangup appears before set_var!"
        );
    }
}

#[tokio::test]
async fn test_hangup_before_set_var_still_works() {
    // Test the new behavior: set_var after hangup should still be processed
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use std::collections::HashMap as StdHashMap;
    use tokio_util::sync::CancellationToken;

    let llm_config = crate::playbook::LlmConfig {
        provider: "test".to_string(),
        ..Default::default()
    };

    let mut sip_config = crate::SipOption::default();
    let mut hangup_headers = StdHashMap::new();
    hangup_headers.insert(
        "X-Hangupreason".to_string(),
        "{{ hangupreason }}".to_string(),
    );
    hangup_headers.insert(
        "X-Skillgroupid".to_string(),
        "{{ skillgroupid }}".to_string(),
    );
    sip_config.hangup_headers = Some(hangup_headers);

    // LLM returns hangup BEFORE set_var (wrong order, but should still work now!)
    let provider = Arc::new(TestProvider::new(vec![
        r#"好的，我帮您转接 <hangup/> <set_var key="hangupreason" value="Transfer"/> <set_var key="skillgroupid" value="7084rx000003"/>"#.to_string(),
    ]));

    let rag = Arc::new(RecordingRag::new());
    let interruption = crate::playbook::InterruptionConfig::default();

    let mut app_config = Config::default();
    app_config.udp_port = 0;

    let app_state = AppStateBuilder::new()
        .with_config(app_config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = CancellationToken::new();
    let session_id = "test-session-hangup-before-setvar".to_string();
    let track_config = TrackConfig::default();

    let initial_extras = StdHashMap::new();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id,
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        Some(initial_extras),
        None,
    ));

    let mut handler = LlmHandler::with_provider(
        llm_config,
        provider,
        rag,
        interruption,
        None,
        StdHashMap::new(),
        None,
        None,
        None,
        Some(sip_config),
    );
    handler.call = Some(active_call.clone());

    // Generate response
    let commands = handler.generate_response().await.unwrap();

    println!("Generated commands: {:?}", commands);

    // Check if variables were set DESPITE hangup coming first
    let state = active_call.call_state.read().await;
    if let Some(extras) = &state.extras {
        println!("Extras after generate_response: {:?}", extras);

        assert_eq!(
            extras.get("hangupreason").and_then(|v| v.as_str()),
            Some("Transfer"),
            "hangupreason should be set even though hangup came first"
        );

        assert_eq!(
            extras.get("skillgroupid").and_then(|v| v.as_str()),
            Some("7084rx000003"),
            "skillgroupid should be set even though hangup came first"
        );
    } else {
        panic!("extras should not be None!");
    }
    drop(state);

    // Render BYE headers
    let rendered_headers = handler.render_sip_headers().await;

    assert!(rendered_headers.is_some());
    let headers = rendered_headers.unwrap();

    println!("Rendered BYE headers: {:?}", headers);

    // The headers should now contain the set values
    assert_eq!(headers.get("X-Hangupreason").unwrap(), "Transfer");
    assert_eq!(headers.get("X-Skillgroupid").unwrap(), "7084rx000003");
}
#[tokio::test]
async fn test_dynamic_scene_prompt_rendering() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use crate::playbook::Scene;

    let mut config = Config::default();
    config.udp_port = 0;
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = tokio_util::sync::CancellationToken::new();
    let session_id = "test-dynamic-prompt".to_string();
    let track_config = TrackConfig::default();

    // Initialize extras with SIP headers and built-in variables
    let mut initial_extras = std::collections::HashMap::new();
    initial_extras.insert("X-Jobid".to_string(), serde_json::json!("JOB-456"));
    initial_extras.insert(
        "_sip_header_keys".to_string(),
        serde_json::json!(["X-Jobid"]),
    );

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id.clone(),
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        Some(initial_extras),
        None,
    ));

    // Create scenes with raw_prompt templates
    let mut scenes = std::collections::HashMap::new();
    scenes.insert(
        "greeting".to_string(),
        Scene {
            id: "greeting".to_string(),
            raw_prompt: Some(
                "您好，客户意图：{{ intent }}\nJob ID：{{ sip[\"X-Jobid\"] }}".to_string(),
            ),
            prompt: "您好，客户意图：\nJob ID：JOB-456".to_string(),
            ..Default::default()
        },
    );
    scenes.insert(
        "detail".to_string(),
        Scene {
            id: "detail".to_string(),
            raw_prompt: Some("处理意图：{{ intent }}\n会话：{{ session_id }}".to_string()),
            prompt: "处理意图：\n会话：".to_string(),
            ..Default::default()
        },
    );

    let llm_config = LlmConfig {
        provider: "mock".to_string(),
        prompt: Some("初始prompt".to_string()),
        ..Default::default()
    };

    let provider = Arc::new(TestProvider::new(vec!["好的".to_string()]));
    let mut handler = LlmHandler::with_provider(
        llm_config,
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        scenes,
        None,
        None,
        Some("greeting".to_string()),
        None,
    );
    handler.call = Some(active_call.clone());

    // Simulate set_var: LLM sets intent during conversation
    {
        let mut state = active_call.call_state.write().await;
        let mut extras = state.extras.take().unwrap_or_default();
        extras.insert(
            "intent".to_string(),
            serde_json::Value::String("买零食".to_string()),
        );
        state.extras = Some(extras);
    }

    // Now switch to greeting scene, it should dynamically render with the latest variables
    let commands = handler.switch_to_scene("greeting", false).await.unwrap();
    // The commands should be empty since greeting scene has no play url
    assert!(commands.is_empty());

    // Check that the system prompt was dynamically rendered with intent
    let system_msg = &handler.history[0];
    assert_eq!(system_msg.role, "system");
    assert!(
        system_msg.content.contains("客户意图：买零食"),
        "System prompt should contain dynamically rendered intent, got: {}",
        system_msg.content
    );
    assert!(
        system_msg.content.contains("Job ID：JOB-456"),
        "System prompt should contain SIP header value, got: {}",
        system_msg.content
    );

    // Switch to detail scene, should also render dynamically
    let _ = handler.switch_to_scene("detail", false).await.unwrap();
    let system_msg = &handler.history[0];
    assert!(
        system_msg.content.contains("处理意图：买零食"),
        "Detail scene should have rendered intent, got: {}",
        system_msg.content
    );
    // Built-in session_id should be available
    assert!(
        system_msg
            .content
            .contains(&format!("会话：{}", session_id)),
        "Detail scene should have session_id, got: {}",
        system_msg.content
    );
}

#[tokio::test]
async fn test_dynamic_prompt_with_builtin_vars() {
    use crate::app::AppStateBuilder;
    use crate::call::{ActiveCall, ActiveCallType};
    use crate::config::Config;
    use crate::media::track::TrackConfig;
    use crate::playbook::{BUILTIN_CALL_TYPE, BUILTIN_SESSION_ID, BUILTIN_START_TIME, Scene};

    let mut config = Config::default();
    config.udp_port = 0;
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .build()
        .await
        .expect("Failed to build app state");

    let cancel_token = tokio_util::sync::CancellationToken::new();
    let session_id = "session-builtin-test".to_string();
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token,
        session_id.clone(),
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,
        false,
        None,
        None,
        None,
    ));

    // Verify built-in variables were injected
    {
        let state = active_call.call_state.read().await;
        let extras = state.extras.as_ref().expect("extras should exist");
        assert_eq!(
            extras.get(BUILTIN_SESSION_ID).and_then(|v| v.as_str()),
            Some("session-builtin-test"),
        );
        assert_eq!(
            extras.get(BUILTIN_CALL_TYPE).and_then(|v| v.as_str()),
            Some("sip"),
        );
        assert!(extras.get(BUILTIN_START_TIME).is_some());
    }

    // Create a scene that uses built-in variables
    let mut scenes = std::collections::HashMap::new();
    scenes.insert(
        "main".to_string(),
        Scene {
            id: "main".to_string(),
            raw_prompt: Some("会话：{{ session_id }}\n类型：{{ call_type }}".to_string()),
            prompt: "会话：\n类型：".to_string(),
            ..Default::default()
        },
    );

    let llm_config = LlmConfig {
        provider: "mock".to_string(),
        prompt: Some("test".to_string()),
        ..Default::default()
    };

    let provider = Arc::new(TestProvider::new(vec![]));
    let mut handler = LlmHandler::with_provider(
        llm_config,
        provider,
        Arc::new(NoopRagRetriever),
        crate::playbook::InterruptionConfig::default(),
        None,
        scenes,
        None,
        None,
        Some("main".to_string()),
        None,
    );
    handler.call = Some(active_call.clone());

    let _ = handler.switch_to_scene("main", false).await.unwrap();
    let system_msg = &handler.history[0];
    assert!(
        system_msg
            .content
            .contains(&format!("会话：{}", session_id)),
        "Should contain built-in session_id, got: {}",
        system_msg.content
    );
    assert!(
        system_msg.content.contains("类型：sip"),
        "Should contain built-in call_type, got: {}",
        system_msg.content
    );
}
