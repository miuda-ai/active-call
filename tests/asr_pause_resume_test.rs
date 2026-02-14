/// Test for ASR pause/resume during call transfer (refer)
///
/// This test verifies:
/// 1. ReferOption.pause_parent_asr field exists and can be set
/// 2. ActiveCallState.pending_asr_resume field exists for state tracking
/// 3. MediaStream supports processor add/remove operations
use active_call::{
    ReferOption,
    app::AppStateBuilder,
    call::{ActiveCall, ActiveCallType},
    config::Config,
    media::{engine::StreamEngine, track::TrackConfig},
    transcription::{TranscriptionOption, TranscriptionType},
};
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_refer_option_pause_parent_asr_field() {
    // Test that ReferOption struct has the pause_parent_asr field
    let refer_option = ReferOption {
        pause_parent_asr: Some(true),
        auto_hangup: None,
        denoise: None,
        timeout: None,
        moh: None,
        asr: None,
        sip: None,
        call_id: None,
    };

    assert_eq!(refer_option.pause_parent_asr, Some(true));

    let refer_option_false = ReferOption {
        pause_parent_asr: Some(false),
        auto_hangup: None,
        denoise: None,
        timeout: None,
        moh: None,
        asr: None,
        sip: None,
        call_id: None,
    };

    assert_eq!(refer_option_false.pause_parent_asr, Some(false));

    // Test None value
    let none_refer = ReferOption {
        pause_parent_asr: None,
        auto_hangup: None,
        denoise: None,
        timeout: None,
        moh: None,
        asr: None,
        sip: None,
        call_id: None,
    };
    assert_eq!(none_refer.pause_parent_asr, None);
}

#[tokio::test]
async fn test_active_call_state_has_pending_asr_resume() -> Result<()> {
    // Setup minimal ActiveCall to test state structure
    let mut config = Config::default();
    config.udp_port = 0;
    config.media_cache_path = "/tmp/mediacache".to_string();

    let stream_engine = Arc::new(StreamEngine::default());
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await?;

    let cancel_token = CancellationToken::new();
    let session_id = format!("test-asr-pause-{}", uuid::Uuid::new_v4());
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token.clone(),
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

    // Test that pending_asr_resume field exists and can be set
    {
        let mut state = active_call.call_state.write().await;
        let asr_option = TranscriptionOption {
            provider: Some(TranscriptionType::Aliyun),
            ..Default::default()
        };
        state.pending_asr_resume = Some((12345u32, asr_option.clone()));

        assert!(state.pending_asr_resume.is_some());
        let (ssrc, option) = state.pending_asr_resume.as_ref().unwrap();
        assert_eq!(*ssrc, 12345u32);
        assert!(option.provider.is_some());
    }

    // Test that it can be taken (moved out)
    {
        let mut state = active_call.call_state.write().await;
        let taken = state.pending_asr_resume.take();
        assert!(taken.is_some());
        assert!(state.pending_asr_resume.is_none());
    }

    Ok(())
}

#[tokio::test]
async fn test_refer_option_serialization() -> Result<()> {
    // Test that ReferOption can be serialized/deserialized with pause_parent_asr
    use serde_json;

    let refer_option = ReferOption {
        pause_parent_asr: Some(true),
        auto_hangup: Some(false),
        denoise: None,
        timeout: None,
        moh: None,
        asr: None,
        sip: None,
        call_id: None,
    };

    let json = serde_json::to_string(&refer_option)?;
    assert!(json.contains("pauseParentAsr"));

    let deserialized: ReferOption = serde_json::from_str(&json)?;
    assert_eq!(deserialized.pause_parent_asr, Some(true));
    assert_eq!(deserialized.auto_hangup, Some(false));

    Ok(())
}

#[tokio::test]
async fn test_media_stream_processor_operations() -> Result<()> {
    use active_call::media::AudioFrame;
    use active_call::media::processor::Processor;
    use active_call::media::stream::MediaStreamBuilder;

    // Define a test processor type
    struct AsrTestProcessor {
        #[allow(unused)]
        id: String,
    }

    impl Processor for AsrTestProcessor {
        fn process_frame(&mut self, _frame: &mut AudioFrame) -> Result<()> {
            // Simulate ASR processing
            Ok(())
        }
    }

    let event_sender = active_call::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    let track_id = "asr-test-track".to_string();

    // Test that we can create a processor (compilation test)
    let processor = Box::new(AsrTestProcessor {
        id: "asr-1".to_string(),
    });

    // Test append_processor API exists and returns Result
    let append_result = stream.append_processor(&track_id, processor).await;
    // Will fail because track doesn't exist, but that's expected
    assert!(append_result.is_err());

    // Test remove_processor API exists and returns Result
    let remove_result = stream.remove_processor::<AsrTestProcessor>(&track_id).await;
    // Will fail because track doesn't exist, but that's expected
    assert!(remove_result.is_err());

    Ok(())
}

#[tokio::test]
async fn test_pending_asr_resume_lifecycle() -> Result<()> {
    // Test the full lifecycle of pending_asr_resume state
    let mut config = Config::default();
    config.udp_port = 0;
    config.media_cache_path = "/tmp/mediacache".to_string();

    let stream_engine = Arc::new(StreamEngine::default());
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await?;

    let cancel_token = CancellationToken::new();
    let session_id = format!("test-lifecycle-{}", uuid::Uuid::new_v4());
    let track_config = TrackConfig::default();

    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel_token.clone(),
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

    // Simulate refer with pause_parent_asr
    let refer_ssrc = 99999u32;

    #[cfg(feature = "offline")]
    let asr_provider = TranscriptionType::Sensevoice;
    #[cfg(not(feature = "offline"))]
    let asr_provider = TranscriptionType::Aliyun;

    let asr_option = TranscriptionOption {
        provider: Some(asr_provider.clone()),
        ..Default::default()
    };

    // 1. Store pending resume state (simulating what do_refer does)
    {
        let mut state = active_call.call_state.write().await;
        state.pending_asr_resume = Some((refer_ssrc, asr_option.clone()));
    }

    // 2. Verify state is stored
    {
        let state = active_call.call_state.read().await;
        assert!(state.pending_asr_resume.is_some());
        let (stored_ssrc, stored_option) = state.pending_asr_resume.as_ref().unwrap();
        assert_eq!(*stored_ssrc, refer_ssrc);
        assert_eq!(stored_option.provider, Some(asr_provider.clone()));
    }

    // 3. Simulate refer hangup - take and process the pending resume
    {
        let mut state = active_call.call_state.write().await;
        if let Some((ssrc, option)) = state.pending_asr_resume.take() {
            assert_eq!(ssrc, refer_ssrc);
            assert_eq!(option.provider, Some(asr_provider));
            // In real code, this is where we'd recreate the ASR processor
        }
        assert!(state.pending_asr_resume.is_none());
    }

    Ok(())
}
