use active_call::app::AppStateBuilder;
use active_call::call::{ActiveCall, ActiveCallType};
use active_call::config::Config;
use active_call::event::SessionEvent;
use active_call::media::engine::StreamEngine;
use active_call::media::track::TrackConfig;
use active_call::playbook::dialogue::DialogueHandler;
use active_call::playbook::handler::rag::NoopRagRetriever;
use active_call::playbook::handler::{LlmProvider, LlmStreamEvent};
use active_call::playbook::{ChatMessage, InterruptionConfig, LlmConfig};
use active_call::SipOption;
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
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
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LlmStreamEvent>> + Send>>> {
        let response = self.response.clone();
        let stream = async_stream::try_stream! {
            yield LlmStreamEvent::Content(response);
        };
        Ok(Box::pin(stream))
    }
}

/// 测试当 LLM 返回 <hangup/> 时，配置的 SIP headers 被正确存储到 ActiveCallState.extras
#[tokio::test]
async fn test_autohangup_headers_stored_in_extras() -> Result<()> {
    // Setup
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
        "test-hangup-headers".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    let mut hangup_headers = HashMap::new();
    hangup_headers.insert("X-Test-Header".to_string(), "test-value".to_string());
    hangup_headers.insert("X-Job-Id".to_string(), "job-123".to_string());

    let sip_option = SipOption {
        hangup_headers: Some(hangup_headers),
        ..Default::default()
    };

    let llm_config = LlmConfig::default();

    let response_text = "Goodbye <hangup/>";
    let provider = Arc::new(MockLlmProvider {
        response: response_text.to_string(),
    });

    let mut handler = active_call::playbook::handler::LlmHandler::with_provider(
        llm_config,
        provider,
        Arc::new(NoopRagRetriever),
        InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        Some(sip_option),
    );

    handler.set_call(active_call.clone());

    // Act: Simulate user speech that triggers LLM response with <hangup/>
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        index: 0,
        text: "Hello".to_string(),
        start_time: None,
        end_time: None,
        is_filler: None,
        confidence: None,
        task_id: None,
        timestamp: 0,
        refer: None,
    };

    let _ = handler.on_event(&event).await?;

    // Assert: Check that headers are stored in extras
    {
        let state = active_call.call_state.read().await;
        let extras = state
            .extras
            .as_ref()
            .expect("extras should be present after hangup");

        let header_val = extras
            .get("_hangup_headers")
            .expect("_hangup_headers should be in extras");
        let headers: HashMap<String, String> =
            serde_json::from_value(header_val.clone()).unwrap();

        assert_eq!(
            headers.get("X-Test-Header"),
            Some(&"test-value".to_string()),
            "X-Test-Header should match configured value"
        );
        assert_eq!(
            headers.get("X-Job-Id"),
            Some(&"job-123".to_string()),
            "X-Job-Id should match configured value"
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_autohangup_without_sip_config() -> Result<()> {
    // Setup: No SipOption
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
        "test-no-sip-config".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    let llm_config = LlmConfig::default();

    let response_text = "Goodbye <hangup/>";
    let provider = Arc::new(MockLlmProvider {
        response: response_text.to_string(),
    });

    let mut handler = active_call::playbook::handler::LlmHandler::with_provider(
        llm_config,
        provider,
        Arc::new(NoopRagRetriever),
        InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        None, // No SipOption
    );

    handler.set_call(active_call.clone());

    // Act
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        index: 0,
        text: "Hello".to_string(),
        start_time: None,
        end_time: None,
        is_filler: None,
        confidence: None,
        task_id: None,
        timestamp: 0,
        refer: None,
    };

    let _ = handler.on_event(&event).await?;

    // Assert: _hangup_headers should either not exist or be empty
    {
        let state = active_call.call_state.read().await;
        if let Some(extras) = &state.extras {
            if let Some(header_val) = extras.get("_hangup_headers") {
                let headers: HashMap<String, String> =
                    serde_json::from_value(header_val.clone()).unwrap_or_default();
                assert!(
                    headers.is_empty(),
                    "Headers should be empty when no SipOption is provided"
                );
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_autohangup_headers_with_template_variables() -> Result<()> {
    // Setup
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
        "test-headers-template".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Pre-populate extras with variables
    {
        let mut state = active_call.call_state.write().await;
        let mut extras = HashMap::new();
        extras.insert(
            "call_result".to_string(),
            serde_json::Value::String("success".to_string()),
        );
        state.extras = Some(extras);
    }

    let mut hangup_headers = HashMap::new();
    hangup_headers.insert(
        "X-Call-Result".to_string(),
        "{{ call_result }}".to_string(),
    );

    let sip_option = SipOption {
        hangup_headers: Some(hangup_headers),
        ..Default::default()
    };

    let llm_config = LlmConfig::default();

    let response_text = "Done <hangup/>";
    let provider = Arc::new(MockLlmProvider {
        response: response_text.to_string(),
    });

    let mut handler = active_call::playbook::handler::LlmHandler::with_provider(
        llm_config,
        provider,
        Arc::new(NoopRagRetriever),
        InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        None,
        None,
        Some(sip_option),
    );

    handler.set_call(active_call.clone());

    // Act
    let event = SessionEvent::AsrFinal {
        track_id: "test".to_string(),
        index: 0,
        text: "Hello".to_string(),
        start_time: None,
        end_time: None,
        is_filler: None,
        confidence: None,
        task_id: None,
        timestamp: 0,
        refer: None,
    };

    let _ = handler.on_event(&event).await?;

    // Assert: Template variable should be rendered
    {
        let state = active_call.call_state.read().await;
        let extras = state.extras.as_ref().expect("extras should be present");

        let header_val = extras
            .get("_hangup_headers")
            .expect("_hangup_headers should be in extras");
        let headers: HashMap<String, String> =
            serde_json::from_value(header_val.clone()).unwrap();

        assert_eq!(
            headers.get("X-Call-Result"),
            Some(&"success".to_string()),
            "Template variable should be rendered"
        );
    }

    Ok(())
}
