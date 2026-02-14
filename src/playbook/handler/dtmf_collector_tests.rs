use super::*;
use crate::call::Command;
use crate::event::SessionEvent;
use anyhow::Result;
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

/// Test helper to create a minimal handler with DTMF collectors
fn create_test_handler(
    collectors: Option<HashMap<String, super::super::DtmfCollectorConfig>>,
) -> LlmHandler {
    let provider = Arc::new(NoopLlmProvider);
    LlmHandler::with_provider(
        LlmConfig::default(),
        provider,
        Arc::new(NoopRagRetriever),
        super::super::InterruptionConfig::default(),
        None,
        HashMap::new(),
        None,
        collectors,
        None,
        None,
    )
}

struct NoopLlmProvider;

#[async_trait::async_trait]
impl LlmProvider for NoopLlmProvider {
    async fn call(&self, _config: &LlmConfig, _history: &[ChatMessage]) -> Result<String> {
        Ok("Hello".to_string())
    }

    async fn call_stream(
        &self,
        _config: &LlmConfig,
        _history: &[ChatMessage],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LlmStreamEvent>> + Send>>> {
        let s = async_stream::stream! {
            yield Ok(LlmStreamEvent::Content("Hello".to_string()));
        };
        Ok(Box::pin(s))
    }
}

fn create_phone_collector() -> super::super::DtmfCollectorConfig {
    super::super::DtmfCollectorConfig {
        description: Some("11-digit phone number".to_string()),
        digits: Some(11),
        min_digits: None,
        max_digits: None,
        finish_key: Some("#".to_string()),
        timeout: Some(20),
        inter_digit_timeout: Some(5),
        validation: Some(super::super::DtmfValidation {
            pattern: r"^1[3-9]\d{9}$".to_string(),
            error_message: Some(
                "Please enter a valid 11-digit phone number starting with 1".to_string(),
            ),
        }),
        retry_times: Some(3),
        interruptible: Some(false),
    }
}

fn create_code_collector() -> super::super::DtmfCollectorConfig {
    super::super::DtmfCollectorConfig {
        description: Some("6-digit verification code".to_string()),
        digits: Some(6),
        min_digits: None,
        max_digits: None,
        finish_key: None, // Auto-complete at 6 digits
        timeout: Some(30),
        inter_digit_timeout: Some(5),
        validation: None,
        retry_times: Some(2),
        interruptible: Some(false),
    }
}

#[test]
fn test_generate_collector_instructions_empty() {
    let instructions = LlmHandler::generate_collector_instructions(None);
    assert_eq!(instructions, "");

    let empty_map: HashMap<String, super::super::DtmfCollectorConfig> = HashMap::new();
    let instructions = LlmHandler::generate_collector_instructions(Some(&empty_map));
    assert_eq!(instructions, "");
}

#[test]
fn test_generate_collector_instructions_with_collectors() {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());
    collectors.insert("code".to_string(), create_code_collector());

    let instructions = LlmHandler::generate_collector_instructions(Some(&collectors));

    // Should contain header
    assert!(instructions.contains("### DTMF Digit Collection"));
    assert!(instructions.contains("use the DTMF digit collection command"));

    // Should contain usage
    assert!(
        instructions.contains("<collect type=\"TYPE\" var=\"VAR_NAME\" prompt=\"PROMPT_TEXT\" />")
    );

    // Should list both collectors (sorted alphabetically)
    assert!(instructions.contains("`code`: 6-digit verification code"));
    assert!(instructions.contains("`phone`: 11-digit phone number"));

    // Should show config details
    assert!(instructions.contains("11 digits"));
    assert!(instructions.contains("press # to finish"));
    assert!(instructions.contains("6 digits")); // Only 'digits', no finish key

    // Should contain flow description
    assert!(instructions.contains("When collection completes"));
    assert!(instructions.contains("{{ var_name }}"));
}

#[test]
fn test_start_collector_success() {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let mut handler = create_test_handler(Some(collectors));

    assert!(!handler.is_collecting());
    assert!(handler.start_collector("phone", "user_phone"));
    assert!(handler.is_collecting());

    let state = handler.collector_state.as_ref().unwrap();
    assert_eq!(state.collector_type, "phone");
    assert_eq!(state.var_name, "user_phone");
    assert_eq!(state.buffer, "");
    assert_eq!(state.retry_count, 0);
    assert_eq!(state.config.digits, Some(11));
}

#[test]
fn test_start_collector_invalid_type() {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let mut handler = create_test_handler(Some(collectors));

    assert!(!handler.start_collector("unknown_type", "var"));
    assert!(!handler.is_collecting());
}

#[test]
fn test_start_collector_no_collectors_configured() {
    let mut handler = create_test_handler(None);

    assert!(!handler.start_collector("phone", "var"));
    assert!(!handler.is_collecting());
}

#[tokio::test]
async fn test_handle_collector_digit_accumulation() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("code".to_string(), create_code_collector());

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("code", "verification_code");

    // Send digits one by one
    let commands = handler.handle_collector_digit("1").await?;
    assert!(commands.is_empty());
    assert_eq!(handler.collector_state.as_ref().unwrap().buffer, "1");

    let commands = handler.handle_collector_digit("2").await?;
    assert!(commands.is_empty());
    assert_eq!(handler.collector_state.as_ref().unwrap().buffer, "12");

    let commands = handler.handle_collector_digit("3").await?;
    assert!(commands.is_empty());
    assert_eq!(handler.collector_state.as_ref().unwrap().buffer, "123");

    Ok(())
}

#[tokio::test]
async fn test_handle_collector_digit_finish_key() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("phone", "user_phone");

    // Accumulate some digits
    handler.handle_collector_digit("1").await?;
    handler.handle_collector_digit("3").await?;
    handler.handle_collector_digit("8").await?;

    assert_eq!(handler.collector_state.as_ref().unwrap().buffer, "138");

    // Press finish key # - should fail validation (too few digits)
    let _commands = handler.handle_collector_digit("#").await?;

    // Collection should have ended or retrying
    // (depending on retry mechanism, may still be collecting with retry_count incremented)

    Ok(())
}

#[tokio::test]
async fn test_collector_auto_complete_at_max_digits() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("code".to_string(), create_code_collector());

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("code", "verification_code");

    // Send 5 digits - should not complete
    for digit in &["1", "2", "3", "4", "5"] {
        let commands = handler.handle_collector_digit(digit).await?;
        assert!(commands.is_empty());
    }

    assert!(handler.is_collecting());
    assert_eq!(handler.collector_state.as_ref().unwrap().buffer, "12345");

    // Send the 6th digit - should auto-complete since no finish_key is set
    let commands = handler.handle_collector_digit("6").await?;

    // Collection should have ended
    assert!(!handler.is_collecting());

    // Should have generated commands
    assert!(!commands.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_collector_validation_success() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let mut handler = create_test_handler(Some(collectors));

    // Note: Without a real ActiveCall, we can't test variable storage,
    // but we can test that collection completes successfully
    handler.start_collector("phone", "user_phone");

    // Enter a valid phone number
    for digit in "13812345678".chars() {
        handler.handle_collector_digit(&digit.to_string()).await?;
    }

    // Press finish key
    let commands = handler.handle_collector_digit("#").await?;

    // Collection should have ended
    assert!(!handler.is_collecting());

    // Should have notified LLM
    assert!(
        handler
            .history
            .iter()
            .any(|msg| msg.role == "system" && msg.content.contains("DTMF collection completed"))
    );

    // Should have generated commands (to continue conversation)
    assert!(!commands.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_collector_validation_failure_with_retry() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("phone", "user_phone");

    // Enter an invalid phone number (starts with 0 instead of 1)
    for digit in "03812345678".chars() {
        handler.handle_collector_digit(&digit.to_string()).await?;
    }

    // Press finish key - should fail validation
    let commands = handler.handle_collector_digit("#").await?;

    // Collection should still be active (retrying)
    assert!(handler.is_collecting());
    let state = handler.collector_state.as_ref().unwrap();
    assert_eq!(state.retry_count, 1);
    assert_eq!(state.buffer, ""); // Buffer should be reset

    // Should have TTS command with error message
    assert!(!commands.is_empty());
    let first_cmd = &commands[0];
    if let Command::Tts { text, .. } = first_cmd {
        assert!(text.contains("valid 11-digit phone number"));
    } else {
        panic!("Expected TTS command for retry");
    }

    Ok(())
}

#[tokio::test]
async fn test_collector_max_retries_exceeded() -> Result<()> {
    let mut collectors = HashMap::new();
    let mut collector = create_phone_collector();
    collector.retry_times = Some(2); // Only 2 retries
    collectors.insert("phone".to_string(), collector);

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("phone", "user_phone");

    // Simulate 3 failed attempts (0, 1, 2 retries)
    for retry in 0..=2 {
        if retry > 0 {
            // Restart collector manually to simulate retry
            handler.start_collector("phone", "user_phone");
            let state = handler.collector_state.as_mut().unwrap();
            state.retry_count = retry;
        }

        // Enter invalid number
        for digit in "00000000000".chars() {
            handler.handle_collector_digit(&digit.to_string()).await?;
        }

        // Press finish key
        let _commands = handler.handle_collector_digit("#").await?;

        if retry < 2 {
            // Should still be collecting (retrying)
            assert!(handler.is_collecting());
        } else {
            // Max retries reached - should stop collecting
            assert!(!handler.is_collecting());

            // Should notify LLM of failure
            assert!(handler.history.iter().any(|msg| msg.role == "system"
                && msg.content.contains("DTMF collection failed")
                && msg.content.contains("after 2 retries")));
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_collector_overall_timeout() -> Result<()> {
    let mut collectors = HashMap::new();
    let mut collector = create_code_collector();
    collector.timeout = Some(1); // 1 second timeout for testing
    collectors.insert("code".to_string(), collector);

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("code", "verification_code");

    // Manually set start time to past
    if let Some(state) = &mut handler.collector_state {
        state.start_time = std::time::Instant::now() - std::time::Duration::from_secs(2);
    }

    // Check timeout
    let _commands = handler.check_collector_timeout().await?;

    // Collection should have ended
    assert!(!handler.is_collecting());

    // Should have notified LLM
    assert!(
        handler
            .history
            .iter()
            .any(|msg| msg.role == "system" && msg.content.contains("timed out"))
    );

    Ok(())
}

#[tokio::test]
async fn test_collector_inter_digit_timeout() -> Result<()> {
    let mut collectors = HashMap::new();
    let mut collector = create_code_collector();
    collector.inter_digit_timeout = Some(1); // 1 second inter-digit timeout
    collectors.insert("code".to_string(), collector);

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("code", "verification_code");

    // Enter some digits (not enough for the required 6 digits)
    handler.handle_collector_digit("1").await?;
    handler.handle_collector_digit("2").await?;
    handler.handle_collector_digit("3").await?;

    // Manually set last digit time to past
    if let Some(state) = &mut handler.collector_state {
        state.last_digit_time = std::time::Instant::now() - std::time::Duration::from_secs(2);
    }

    // Check timeout
    let _commands = handler.check_collector_timeout().await?;

    // Collection should have triggered retry (not enough digits: 3 < 6)
    // After first retry, it should still be collecting
    assert!(handler.is_collecting());
    let state = handler.collector_state.as_ref().unwrap();
    assert_eq!(state.retry_count, 1); // First retry

    Ok(())
}

#[tokio::test]
async fn test_collector_on_event_dtmf_routing() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("code".to_string(), create_code_collector());

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("code", "verification_code");

    // Send DTMF event - should route to collector
    let event = SessionEvent::Dtmf {
        digit: "5".to_string(),
        track_id: "test-track".to_string(),
        timestamp: crate::media::get_timestamp(),
    };

    let _commands = handler.on_event(&event).await?;

    // Digit should be accumulated
    assert_eq!(handler.collector_state.as_ref().unwrap().buffer, "5");

    Ok(())
}

#[tokio::test]
async fn test_collector_on_event_ignores_asr() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("code".to_string(), create_code_collector());

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("code", "verification_code");

    // Send ASR event during collection - should be ignored
    let event = SessionEvent::AsrFinal {
        text: "hello".to_string(),
        track_id: "test-track".to_string(),
        timestamp: crate::media::get_timestamp(),
        index: 0,
        start_time: None,
        end_time: None,
        is_filler: None,
        confidence: None,
        task_id: None,
    };

    let commands = handler.on_event(&event).await?;

    // Should return no commands (ignored)
    assert!(commands.is_empty());

    // Should still be collecting
    assert!(handler.is_collecting());

    Ok(())
}

#[tokio::test]
async fn test_collector_on_event_allows_hangup() -> Result<()> {
    let mut collectors = HashMap::new();
    collectors.insert("code".to_string(), create_code_collector());

    let mut handler = create_test_handler(Some(collectors));
    handler.start_collector("code", "verification_code");

    // Send hangup event during collection - should clear collection and pass through
    let event = SessionEvent::Hangup {
        track_id: "test-track".to_string(),
        timestamp: crate::media::get_timestamp(),
        reason: None,
        initiator: None,
        start_time: "2026-02-14T00:00:00Z".to_string(),
        hangup_time: "2026-02-14T00:00:10Z".to_string(),
        answer_time: None,
        ringing_time: None,
        from: None,
        to: None,
        extra: None,
        refer: None,
    };

    let _commands = handler.on_event(&event).await?;

    // Collection should have ended
    assert!(!handler.is_collecting());

    Ok(())
}

#[tokio::test]
async fn test_extract_collect_command_from_stream() {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let mut handler = create_test_handler(Some(collectors));

    let mut buffer = String::from(
        "好的，<collect type=\"phone\" var=\"user_phone\" prompt=\"请输入您的11位手机号码\" />我会记录您的信息。",
    );
    let commands = handler
        .extract_streaming_commands(&mut buffer, "test-play-id", false)
        .await;

    // Should have extracted the collect command
    assert!(handler.is_collecting());

    // Buffer should have text after the collect tag removed during flush
    // (the implementation flushes text before the tag and removes the tag itself)

    // Should have TTS command for prefix text
    assert!(!commands.is_empty());

    // Should have TTS command for the prompt
    let has_prompt_tts = commands.iter().any(|cmd| {
        if let Command::Tts { text, .. } = cmd {
            text.contains("请输入您的11位手机号码")
        } else {
            false
        }
    });
    assert!(has_prompt_tts);
}

#[tokio::test]
async fn test_extract_collect_command_unknown_type() {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let mut handler = create_test_handler(Some(collectors));

    let mut buffer = String::from("<collect type=\"unknown\" var=\"test\" prompt=\"test\" />");
    let _commands = handler
        .extract_streaming_commands(&mut buffer, "test-play-id", false)
        .await;

    // Should not start collecting
    assert!(!handler.is_collecting());

    // Should have notified LLM via history
    assert!(
        handler
            .history
            .iter()
            .any(|msg| msg.role == "system" && msg.content.contains("Unknown DTMF collector type"))
    );
}

#[test]
fn test_build_system_prompt_includes_collector_instructions() {
    let mut collectors = HashMap::new();
    collectors.insert("phone".to_string(), create_phone_collector());

    let config = LlmConfig {
        prompt: Some("Base prompt".to_string()),
        ..Default::default()
    };

    let prompt = LlmHandler::build_system_prompt(&config, None, Some(&collectors));

    assert!(prompt.contains("Base prompt"));
    assert!(prompt.contains("### DTMF Digit Collection"));
    assert!(prompt.contains("<collect type=\"TYPE\" var=\"VAR_NAME\""));
    assert!(prompt.contains("`phone`: 11-digit phone number"));
}

#[test]
fn test_playbook_config_deserialize_with_dtmf_collectors() {
    let yaml = r##"
llm:
  provider: openai
  model: gpt-4

dtmfCollectors:
  phone:
    description: "11-digit phone number"
    digits: 11
    finishKey: "#"
    timeout: 20
    interDigitTimeout: 5
    validation:
      pattern: "^1[3-9]\\d{9}$"
      errorMessage: "Please enter a valid phone number"
    retryTimes: 3
    interruptible: false
  
  code:
    description: "6-digit verification code"
    digits: 6
    timeout: 30
    interDigitTimeout: 5
    retryTimes: 2
"##;

    let config: super::super::PlaybookConfig =
        serde_yaml::from_str(yaml).expect("Failed to parse YAML");

    let llm = config.llm.expect("llm should be present");
    assert_eq!(llm.provider, "openai");

    let collectors = config
        .dtmf_collectors
        .expect("dtmf_collectors should be present");
    assert_eq!(collectors.len(), 2);

    let phone = collectors
        .get("phone")
        .expect("phone collector should exist");
    assert_eq!(phone.description, Some("11-digit phone number".to_string()));
    assert_eq!(phone.digits, Some(11));
    assert_eq!(phone.finish_key, Some("#".to_string()));
    assert_eq!(phone.timeout, Some(20));
    assert_eq!(phone.inter_digit_timeout, Some(5));
    assert_eq!(phone.retry_times, Some(3));
    assert_eq!(phone.interruptible, Some(false));

    let validation = phone.validation.as_ref().expect("validation should exist");
    assert_eq!(validation.pattern, r"^1[3-9]\d{9}$");
    assert_eq!(
        validation.error_message,
        Some("Please enter a valid phone number".to_string())
    );

    let code = collectors.get("code").expect("code collector should exist");
    assert_eq!(code.digits, Some(6));
    assert_eq!(code.finish_key, None); // Not specified
}
