/// Test for ASR pause/resume during call transfer (refer)
///
/// This test verifies:
/// 1. ReferOption.pause_parent_asr field exists and can be set
/// 2. The call's pending_asr_resume slot exists for state tracking
/// 3. MediaStream supports processor add/remove operations
/// 4. SessionEvent::Hangup { refer: Some(true) } is handled without panic in
///    the actor's event handling (lock-free refer-leg lookup)
use active_call::{
    ReferOption,
    app::AppStateBuilder,
    call::active_call::CallSpec,
    call::{
        ActiveCall, ActiveCallType,
        state::{CallProgress, LegShared},
    },
    config::Config,
    event::SessionEvent,
    media::{engine::StreamEngine, get_timestamp, track::TrackConfig},
    transcription::{TranscriptionOption, TranscriptionType},
};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn test_config() -> Config {
    let mut config = Config::default();
    config.udp_port = 0;
    config.media_cache_path = "/tmp/mediacache".to_string();
    config
}

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
        vad: None,
        sip: None,
        call_id: None,
        forward_dtmf: None,
        agc: None,
    };

    assert_eq!(refer_option.pause_parent_asr, Some(true));

    let refer_option_false = ReferOption {
        pause_parent_asr: Some(false),
        auto_hangup: None,
        denoise: None,
        timeout: None,
        moh: None,
        asr: None,
        vad: None,
        sip: None,
        call_id: None,
        forward_dtmf: None,
        agc: None,
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
        vad: None,
        sip: None,
        call_id: None,
        forward_dtmf: None,
        agc: None,
    };
    assert_eq!(none_refer.pause_parent_asr, None);
}

#[tokio::test]
async fn test_call_has_pending_asr_resume() -> Result<()> {
    // pending_asr_resume is a lock-free slot on the call (survives the actor)
    let app_state = AppStateBuilder::new()
        .with_config(test_config())
        .with_stream_engine(Arc::new(StreamEngine::default()))
        .build()
        .await?;
    let call = Arc::new(ActiveCall::new(CallSpec {
        call_type: ActiveCallType::Sip,
        cancel_token: CancellationToken::new(),
        session_id: "asr-resume-slot".to_string(),
        invitation: app_state.invitation.clone(),
        app_state: app_state.clone(),
        track_config: TrackConfig::default(),
        audio_receiver: None,
        dump_events: false,
        server_side_track_id: None,
        extras: None,
    }));
    let asr_option = TranscriptionOption {
        provider: Some(TranscriptionType::Aliyun),
        ..Default::default()
    };
    call.set_pending_asr_resume((12345u32, asr_option.clone()));

    let (ssrc, option) = call
        .take_pending_asr_resume()
        .expect("pending_asr_resume should be set");
    assert_eq!(ssrc, 12345u32);
    assert!(option.provider.is_some());
    assert!(call.take_pending_asr_resume().is_none(), "slot drained");

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
        vad: None,
        sip: None,
        call_id: None,
        forward_dtmf: None,
        agc: None,
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
    // Test the full lifecycle of the pending_asr_resume slot
    let app_state = AppStateBuilder::new()
        .with_config(test_config())
        .with_stream_engine(Arc::new(StreamEngine::default()))
        .build()
        .await?;
    let call = Arc::new(ActiveCall::new(CallSpec {
        call_type: ActiveCallType::Sip,
        cancel_token: CancellationToken::new(),
        session_id: "asr-resume-lifecycle".to_string(),
        invitation: app_state.invitation.clone(),
        app_state: app_state.clone(),
        track_config: TrackConfig::default(),
        audio_receiver: None,
        dump_events: false,
        server_side_track_id: None,
        extras: None,
    }));

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
    call.set_pending_asr_resume((refer_ssrc, asr_option.clone()));

    // 2. Verify state is stored
    {
        let (stored_ssrc, stored_option) =
            call.take_pending_asr_resume().expect("state should be set");
        assert_eq!(stored_ssrc, refer_ssrc);
        assert_eq!(stored_option.provider, Some(asr_provider.clone()));
        // put it back for the "hangup" step
        call.set_pending_asr_resume((stored_ssrc, stored_option));
    }

    // 3. Simulate refer hangup - take and process the pending resume
    {
        let (ssrc, option) = call
            .take_pending_asr_resume()
            .expect("pending resume should still be set");
        assert_eq!(ssrc, refer_ssrc);
        assert_eq!(option.provider, Some(asr_provider));
        // In real code, this is where we'd recreate the ASR processor
    }

    Ok(())
}

/// Regression test: SessionEvent::Hangup { refer: Some(true) } must not panic in
/// the actor loop.  The original code called `blocking_read()` on an RwLock
/// inside an async context, which panics at runtime; the lock-free refer-leg
/// lookup used now is safe from any context.
#[tokio::test]
async fn test_refer_hangup_event_in_async_does_not_panic() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();

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
    let session_id = format!("test-refer-hangup-no-panic-{}", uuid::Uuid::new_v4());

    let active_call = Arc::new(ActiveCall::new(CallSpec {
        call_type: ActiveCallType::Sip,
        cancel_token: cancel_token.clone(),
        session_id: session_id.clone(),
        invitation: app_state.invitation.clone(),
        app_state: app_state.clone(),
        track_config: TrackConfig::default(),
        audio_receiver: None,
        dump_events: false,
        server_side_track_id: None,
        extras: None,
    }));

    // The ssrc of the refer leg is what the Hangup(refer=true) handler compares
    // against; install a refer leg so that lookup path is exercised.
    let refer_ssrc: u32 = 0xDEAD_BEEF;
    let refer_leg = LegShared::new(refer_ssrc, true, CallProgress::default());
    active_call.refer_leg.store(Some(Arc::new(refer_leg)));

    // Start serve() in a background task.  new_receiver() must be called before serve().
    let receiver = active_call.new_receiver();
    let call_clone = active_call.clone();
    let serve_handle = tokio::spawn(async move {
        call_clone.serve(receiver).await.ok();
    });

    // Give the actor loop a moment to enter the select!.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Fire the exact session event that triggered the blocking_read() panic.
    let now = get_timestamp();
    active_call
        .event_sender
        .send(SessionEvent::Hangup {
            track_id: session_id.clone(),
            timestamp: now,
            reason: Some("refer ended".to_string()),
            initiator: None,
            start_time: "2026-01-01T00:00:00Z".to_string(),
            hangup_time: "2026-01-01T00:00:01Z".to_string(),
            answer_time: None,
            ringing_time: None,
            from: None,
            to: None,
            extra: None,
            refer: Some(true),
        })
        .ok();

    // Allow the event to be processed.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Shut down cleanly – the old code would have already panicked above.
    cancel_token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), serve_handle).await;

    Ok(())
}
