use active_call::app::AppStateBuilder;
use active_call::call::{ActiveCall, ActiveCallType, Command};
use active_call::config::Config;
use active_call::event::SessionEvent;
use active_call::media::engine::StreamEngine;
use active_call::media::track::TrackConfig;
use active_call::playbook::{
    ChatMessage, LlmConfig, PlaybookConfig, PlaybookRunner,
    handler::{LlmHandler, LlmProvider, RagRetriever},
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct MockLlmProvider {
    response: String,
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn call(&self, _config: &LlmConfig, _history: &[ChatMessage]) -> Result<String> {
        Ok(self.response.clone())
    }

    async fn call_stream(
        &self,
        _config: &LlmConfig,
        _history: &[ChatMessage],
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>> {
        let response = self.response.clone();
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
async fn test_playbook_run_flow() -> Result<()> {
    // 1. Setup AppState
    let mut config = Config::default();
    config.udp_port = 0; // Use random port to avoid collision
    let stream_engine = Arc::new(StreamEngine::new());

    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await?;

    // 2. Create ActiveCall
    let cancel_token = CancellationToken::new();
    let session_id = "test-session".to_string();
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token.clone(),
        session_id.clone(),
        app_state.invitation.clone(),
        app_state.clone(),
        track_config,
        None,  // audio_receiver
        false, // dump_events
        None,  // server_side_track
        None,  // extras
    ));

    // Get command receiver
    let receiver = active_call.new_receiver();
    let mut cmd_rx = receiver.cmd_receiver;

    // 3. Setup Handler with Mock Provider
    let llm_config = LlmConfig {
        provider: "mock".to_string(),
        prompt: Some("You are a bot".to_string()),
        greeting: Some("Hello world".to_string()),
        ..Default::default()
    };

    let response_json = r#"{
        "text": "How can I help you?",
        "waitInputTimeout": 5000
    }"#;

    let provider = Arc::new(MockLlmProvider {
        response: response_json.to_string(),
    });
    // Arc<NoopRag> is needed (LlmHandler takes Arc)
    let llm_handler = LlmHandler::with_provider(
        llm_config.clone(),
        provider,
        Arc::new(NoopRag),
        active_call::playbook::InterruptionConfig::default(),
        HashMap::new(),
        None,
        None,
    );

    // 4. Create Runner
    let runner = PlaybookRunner::with_handler(
        Box::new(llm_handler),
        active_call.clone(),
        PlaybookConfig::default(),
    );

    // 5. Run Runner in background
    let join_handle = tokio::spawn(async move {
        runner.run().await;
    });

    // 6. Assert Greeting (on_start)
    // Expect Command::Tts from greeting
    if let Ok(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Tts { text, .. } => assert_eq!(text, "Hello world"),
            _ => panic!("Expected TTS greeting, got {:?}", cmd),
        }
    } else {
        panic!("Did not receive greeting command");
    }

    // Simulate Answer event to let runner proceed to dialogue loop
    active_call.event_sender.send(SessionEvent::Answer {
        track_id: "track1".to_string(),
        timestamp: 0,
        sdp: "".to_string(),
        refer: None,
    })?;

    // 7. Simulate User Input
    let event = SessionEvent::AsrFinal {
        track_id: "track1".to_string(),
        timestamp: 100,
        index: 1,
        start_time: None,
        end_time: None,
        text: "I need help".to_string(),
        is_filler: None,
        confidence: None,
    };

    // Send event
    active_call.event_sender.send(event)?;

    // 8. Assert Response
    if let Ok(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Tts { text, .. } => assert_eq!(text, "How can I help you?"),
            _ => panic!("Expected TTS response, got {:?}", cmd),
        }
    } else {
        panic!("Did not receive response command");
    }

    // Hangup to stop runner loop
    active_call.event_sender.send(SessionEvent::Hangup {
        track_id: "track1".to_string(),
        timestamp: 200,
        reason: None,
        initiator: None,
        start_time: "".to_string(),
        hangup_time: "".to_string(),
        answer_time: None,
        ringing_time: None,
        from: None,
        to: None,
        extra: None,
        refer: None,
    })?;

    join_handle.await?;

    Ok(())
}

#[tokio::test]
async fn test_playbook_hangup_flow() -> Result<()> {
    let mut config = Config::default();
    config.udp_port = 0;
    let stream_engine = Arc::new(StreamEngine::new());
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await?;

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        CancellationToken::new(),
        "test-hangup".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
    ));

    let receiver = active_call.new_receiver();
    let mut cmd_rx = receiver.cmd_receiver;

    let response_json = r#"{
        "text": "Goodbye",
        "tools": [{"name": "hangup", "reason": "user_requested"}]
    }"#;

    let provider = Arc::new(MockLlmProvider {
        response: response_json.to_string(),
    });
    let llm_handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRag),
        active_call::playbook::InterruptionConfig::default(),
        HashMap::new(),
        None,
        None,
    );
    let runner = PlaybookRunner::with_handler(
        Box::new(llm_handler),
        active_call.clone(),
        PlaybookConfig::default(),
    );

    tokio::spawn(async move {
        runner.run().await;
    });

    // 1. Check TTS
    if let Ok(cmd) = cmd_rx.recv().await {
        if let Command::Tts {
            text, auto_hangup, ..
        } = cmd
        {
            assert_eq!(text, "Goodbye");
            // The handler may combine Hangup into TTS auto_hangup
            if auto_hangup == Some(true) {
                return Ok(());
            }
        } else {
            panic!("Expected TTS, got {:?}", cmd);
        }
    }

    // 2. Check Hangup (if not combined)
    if let Ok(cmd) = cmd_rx.recv().await {
        if let Command::Hangup { reason, .. } = cmd {
            assert_eq!(reason, Some("user_requested".to_string()));
        } else {
            panic!("Expected Hangup, got {:?}", cmd);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_playbook_accept_flow() -> Result<()> {
    let mut config = Config::default();
    config.udp_port = 0;
    let stream_engine = Arc::new(StreamEngine::new());
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await?;

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        CancellationToken::new(),
        "test-accept".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
    ));

    let receiver = active_call.new_receiver();
    let mut cmd_rx = receiver.cmd_receiver;

    let response_json = r#"{
        "tools": [{"name": "accept"}]
    }"#;

    let provider = Arc::new(MockLlmProvider {
        response: response_json.to_string(),
    });
    let llm_handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRag),
        active_call::playbook::InterruptionConfig::default(),
        HashMap::new(),
        None,
        None,
    );
    let runner = PlaybookRunner::with_handler(
        Box::new(llm_handler),
        active_call.clone(),
        PlaybookConfig::default(),
    );

    tokio::spawn(async move {
        runner.run().await;
    });

    // Check Accept command
    if let Ok(cmd) = cmd_rx.recv().await {
        assert!(matches!(cmd, Command::Accept { .. }));
    } else {
        panic!("Did not receive Accept command");
    }

    Ok(())
}

#[tokio::test]
async fn test_playbook_reject_flow() -> Result<()> {
    let mut config = Config::default();
    config.udp_port = 0;
    let stream_engine = Arc::new(StreamEngine::new());
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await?;

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        CancellationToken::new(),
        "test-reject".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
    ));

    let receiver = active_call.new_receiver();
    let mut cmd_rx = receiver.cmd_receiver;

    let response_json = r#"{
        "tools": [{"name": "reject", "reason": "busy", "code": 486}]
    }"#;

    let provider = Arc::new(MockLlmProvider {
        response: response_json.to_string(),
    });
    let llm_handler = LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRag),
        active_call::playbook::InterruptionConfig::default(),
        HashMap::new(),
        None,
        None,
    );
    let runner = PlaybookRunner::with_handler(
        Box::new(llm_handler),
        active_call.clone(),
        PlaybookConfig::default(),
    );

    tokio::spawn(async move {
        runner.run().await;
    });

    // Check Reject command
    if let Ok(cmd) = cmd_rx.recv().await {
        if let Command::Reject { reason, code } = cmd {
            assert_eq!(reason, "busy");
            assert_eq!(code, Some(486));
        } else {
            panic!("Expected Reject command, got {:?}", cmd);
        }
    } else {
        panic!("Did not receive Reject command");
    }

    Ok(())
}
