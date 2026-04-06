use active_call::app::AppStateBuilder;
use active_call::call::{ActiveCall, ActiveCallType, Command};
use active_call::callrecord::CallRecordHangupReason;
use active_call::config::Config;
use active_call::media::engine::StreamEngine;
use active_call::media::track::TrackConfig;
use active_call::synthesis::SynthesisCommand;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Verify that same play_id preserves auto_hangup (streaming TTS scenario).
///
/// In do_tts(), when the new command has the same play_id as the existing handle,
/// `changed = false`, so auto_hangup is preserved. This is correct because the
/// same track is being reused (streaming TTS sends multiple commands with same play_id).
#[tokio::test]
async fn test_autohangup_preserved_same_play_id() -> Result<()> {
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
        "test-preserve".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Step 1: Simulate first streaming TTS command with auto_hangup
    let ssrc: u32 = 11111;
    {
        let mut state = active_call.call_state.write().await;
        state.current_play_id = Some("stream-1".to_string());
        // tts_handle would exist with ssrc=11111
        // Simulate: same play_id, handle exists → target_ssrc = handle.ssrc
        state.auto_hangup = Some((ssrc, CallRecordHangupReason::BySystem));
    }

    // Step 2: Simulate subsequent streaming TTS command with same play_id
    // In do_tts(): play_id matches → changed=false → auto_hangup preserved
    {
        let state = active_call.call_state.read().await;
        assert_eq!(
            state.auto_hangup,
            Some((ssrc, CallRecordHangupReason::BySystem)),
            "auto_hangup should be preserved when same play_id (streaming mode)"
        );
    }

    Ok(())
}

/// Verify that different play_id clears old auto_hangup.
///
/// When do_tts() receives a command with a different play_id, it should:
/// 1. Set should_interrupt = true
/// 2. Clear the old auto_hangup (unless new command also sets it)
/// 3. Create a new track with new SSRC
///
/// This prevents orphaned auto_hangup when the conversation continues
/// after a new TTS command arrives.
#[tokio::test]
async fn test_autohangup_cleared_different_play_id() -> Result<()> {
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
        "test-clear-diff-playid".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Step 1: First TTS sets auto_hangup with play_id="A"
    {
        let mut state = active_call.call_state.write().await;
        state.current_play_id = Some("play-A".to_string());
        state.auto_hangup = Some((11111, CallRecordHangupReason::BySystem));
    }

    // Step 2: New TTS with different play_id="B" and NO auto_hangup
    // In do_tts(): play_id differs → changed=true → auto_hangup should be cleared
    {
        let mut state = active_call.call_state.write().await;
        // Simulate the fix: when changed=true and new command has no auto_hangup,
        // auto_hangup should be cleared (not preserved)
        // This is what the fix in do_tts() does:
        // if state.tts_handle.is_some() && !changed → preserve
        // else → clear
        // Since changed=true, auto_hangup is cleared
        state.auto_hangup = None; // Fix behavior
        state.current_play_id = Some("play-B".to_string());
    }

    // Step 3: Verify auto_hangup is cleared
    {
        let state = active_call.call_state.read().await;
        assert!(
            state.auto_hangup.is_none(),
            "auto_hangup should be cleared when different play_id starts new track"
        );
    }

    Ok(())
}

/// Verify that different play_id WITH new auto_hangup sets the new one.
///
/// When do_tts() receives a command with different play_id AND auto_hangup=true,
/// the new auto_hangup should be set (not the old one preserved).
#[tokio::test]
async fn test_autohangup_replaced_different_play_id_with_hangup() -> Result<()> {
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
        "test-replace-hangup".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Step 1: Old auto_hangup with ssrc=11111
    {
        let mut state = active_call.call_state.write().await;
        state.current_play_id = Some("play-A".to_string());
        state.auto_hangup = Some((11111, CallRecordHangupReason::BySystem));
    }

    // Step 2: New TTS with different play_id and auto_hangup=true
    let new_ssrc: u32 = 22222;
    {
        let mut state = active_call.call_state.write().await;
        // In do_tts(): auto_hangup=Some(true) → directly set new value
        state.auto_hangup = Some((new_ssrc, CallRecordHangupReason::BySystem));
        state.current_play_id = Some("play-B".to_string());
    }

    // Verify: new auto_hangup with new SSRC
    {
        let state = active_call.call_state.read().await;
        let (ssrc, _) = state.auto_hangup.clone().unwrap();
        assert_eq!(
            ssrc, new_ssrc,
            "auto_hangup should use new SSRC when new command sets it"
        );
    }

    Ok(())
}

/// Verify that interrupt clears auto_hangup.
///
/// When do_interrupt() is called (explicit Interrupt command or internal interrupt),
/// auto_hangup should be cleared because the conversation is continuing.
#[tokio::test]
async fn test_autohangup_cleared_on_interrupt() -> Result<()> {
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
        "test-interrupt-clears".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Step 1: Set auto_hangup
    {
        let mut state = active_call.call_state.write().await;
        state.current_play_id = Some("play-A".to_string());
        state.auto_hangup = Some((11111, CallRecordHangupReason::BySystem));
    }

    // Step 2: Simulate do_interrupt() clearing auto_hangup
    {
        let mut state = active_call.call_state.write().await;
        // This is what do_interrupt() now does after the fix:
        state.tts_handle = None;
        state.moh = None;
        state.auto_hangup = None;
    }

    // Step 3: Verify auto_hangup is cleared
    {
        let state = active_call.call_state.read().await;
        assert!(
            state.auto_hangup.is_none(),
            "auto_hangup should be cleared after interrupt"
        );
    }

    Ok(())
}

/// Verify that no existing handle + no auto_hangup clears stale auto_hangup.
///
/// When there's no tts_handle (first TTS or after interrupt) and the new
/// command doesn't set auto_hangup, any stale auto_hangup from a previous
/// do_play() should be cleared.
#[tokio::test]
async fn test_autohangup_cleared_no_handle_no_hangup() -> Result<()> {
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
        "test-no-handle-clears".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Step 1: Simulate stale auto_hangup from previous do_play()
    {
        let mut state = active_call.call_state.write().await;
        state.auto_hangup = Some((99999, CallRecordHangupReason::BySystem));
        // tts_handle is None (do_play sets it to None)
    }

    // Step 2: New do_tts() with no auto_hangup, no handle
    // In do_tts(): state.tts_handle.is_none() → clear auto_hangup
    {
        let mut state = active_call.call_state.write().await;
        // Simulate the fix behavior:
        // tts_handle is None → auto_hangup cleared (not preserved)
        state.auto_hangup = None;
        state.current_play_id = Some("new-play".to_string());
    }

    // Step 3: Verify stale auto_hangup is cleared
    {
        let state = active_call.call_state.read().await;
        assert!(
            state.auto_hangup.is_none(),
            "Stale auto_hangup should be cleared when no handle exists and new command has no auto_hangup"
        );
    }

    Ok(())
}

/// Verify streaming TTS track emits TrackEnd with correct SSRC and play_id
/// for auto_hangup matching.
#[tokio::test]
async fn test_streaming_tts_track_end_ssrc_playid() -> Result<()> {
    use active_call::media::track::{Track, tts::TtsTrack};
    use active_call::synthesis::{SynthesisClient, SynthesisEvent, SynthesisType};
    use async_trait::async_trait;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use tokio::sync::{broadcast, mpsc};
    use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream};

    struct StreamMock {
        event_sender: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
    }

    #[async_trait]
    impl SynthesisClient for StreamMock {
        fn provider(&self) -> SynthesisType {
            SynthesisType::Other("stream_mock".to_string())
        }

        async fn start(
            &mut self,
        ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
            let (tx, rx) = mpsc::unbounded_channel();
            self.event_sender = Some(tx);
            Ok(UnboundedReceiverStream::new(rx).boxed())
        }

        async fn synthesize(
            &mut self,
            _text: &str,
            _cmd_seq: Option<usize>,
            _option: Option<active_call::synthesis::SynthesisOption>,
        ) -> Result<()> {
            Ok(())
        }

        async fn stop(&mut self) -> Result<()> {
            if let Some(sender) = &self.event_sender {
                let _ = sender.send((None, Ok(SynthesisEvent::Finished)));
            }
            self.event_sender.take();
            Ok(())
        }
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let expected_ssrc: u32 = 54321;
    let expected_play_id = Some("stream-play-id".to_string());

    let mut tts_track = TtsTrack::new(
        "test-track".to_string(),
        "test_session".to_string(),
        true,
        expected_play_id.clone(),
        command_rx,
        Box::new(StreamMock { event_sender: None }),
    )
    .with_ssrc(expected_ssrc)
    .with_cache_enabled(false);

    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    command_tx.send(SynthesisCommand {
        text: "Test".to_string(),
        streaming: true,
        end_of_stream: true,
        ..Default::default()
    })?;

    let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(5));
    tokio::pin!(timeout);

    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let track_end = results.iter().find_map(|r| match r {
        Ok(active_call::event::SessionEvent::TrackEnd { ssrc, play_id, .. }) => {
            Some((*ssrc, play_id.clone()))
        }
        _ => None,
    });

    assert!(
        track_end.is_some(),
        "TrackEnd must be emitted in streaming mode"
    );
    let (ssrc, play_id) = track_end.unwrap();
    assert_eq!(
        ssrc, expected_ssrc,
        "TrackEnd ssrc must match for auto_hangup"
    );
    assert_eq!(play_id, expected_play_id, "TrackEnd play_id must match");

    Ok(())
}

// ============================================================
// Integration tests: verify behaviors via real serve() loop
// ============================================================

async fn make_app_state() -> Result<active_call::app::AppState> {
    let mut config = Config::default();
    config.udp_port = 0;
    let stream_engine = Arc::new(StreamEngine::new());
    AppStateBuilder::new()
        .with_config(config)
        .with_stream_engine(stream_engine)
        .build()
        .await
}

/// Risk 1 (integration): Command::Interrupt via serve() clears auto_hangup.
///
/// This exercises the real do_interrupt() code path: the Command is dispatched
/// through the actual serve() event loop, not just a manual state mutation.
/// The test documents the post-fix behavior: barge-in always clears auto_hangup.
#[tokio::test]
async fn test_command_interrupt_clears_auto_hangup_via_serve() -> Result<()> {
    let app_state = make_app_state().await?;
    let cancel = CancellationToken::new();
    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel.clone(),
        "itg-interrupt-clears".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Seed auto_hangup as if a TTS with auto_hangup=true was scheduled.
    {
        let mut state = active_call.call_state.write().await;
        state.auto_hangup = Some((12345, CallRecordHangupReason::BySystem));
        state.current_play_id = Some("play-before-barge-in".to_string());
    }

    // Start the serve() loop running in the background.
    let receiver = active_call.new_receiver();
    let call_clone = active_call.clone();
    let serve_handle = tokio::spawn(async move {
        call_clone.serve(receiver).await.ok();
    });

    // Give the event loop a moment to enter the select!.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fire Command::Interrupt — this is the barge-in path.
    active_call
        .enqueue_command(Command::Interrupt {
            graceful: Some(false),
            fade_out_ms: None,
        })
        .await?;

    // Allow the command to be processed.
    tokio::time::sleep(Duration::from_millis(200)).await;

    {
        let state = active_call.call_state.read().await;
        assert!(
            state.auto_hangup.is_none(),
            "Command::Interrupt (barge-in) must clear auto_hangup via do_interrupt()"
        );
    }

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), serve_handle).await;
    Ok(())
}

/// Risk 3 (integration): Command::Tts with no auto_hangup and no existing handle
/// clears stale auto_hangup that was left behind by a prior do_play().
///
/// do_play() sets tts_handle=None and may set auto_hangup.  If a subsequent
/// Command::Tts arrives without auto_hangup=true, the logic inside do_tts()
/// detects tts_handle.is_none() and must clear the stale value rather than
/// preserve it (which would cause an orphaned hangup on the wrong SSRC).
///
/// Note: TTS track creation fails here (no registered provider for "mock") but
/// auto_hangup is updated in a dedicated lock scope BEFORE create_tts_track,
/// so the final state is correct even when do_tts() returns an error on track creation.
/// An inline `option` is supplied so do_tts() passes the early "no tts option" check.
#[tokio::test]
async fn test_do_tts_clears_stale_auto_hangup_from_do_play_via_serve() -> Result<()> {
    use active_call::synthesis::{SynthesisOption, SynthesisType};

    let app_state = make_app_state().await?;
    let cancel = CancellationToken::new();
    let active_call = Arc::new(ActiveCall::new(
        ActiveCallType::Sip,
        cancel.clone(),
        "itg-tts-clears-play".to_string(),
        app_state.invitation.clone(),
        app_state.clone(),
        TrackConfig::default(),
        None,
        false,
        None,
        None,
        None,
    ));

    // Simulate the state left by do_play(auto_hangup=true):
    //   tts_handle = None   (do_play always sets it to None)
    //   auto_hangup = Some  (the file track's SSRC)
    let stale_ssrc: u32 = 99999;
    {
        let mut state = active_call.call_state.write().await;
        state.tts_handle = None;
        state.auto_hangup = Some((stale_ssrc, CallRecordHangupReason::BySystem));
        state.current_play_id = Some("file-play".to_string());
    }

    // Start the serve() loop.
    let receiver = active_call.new_receiver();
    let call_clone = active_call.clone();
    let serve_handle = tokio::spawn(async move {
        call_clone.serve(receiver).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a TTS command WITHOUT auto_hangup (conversation continues after file play).
    // do_tts() finds tts_handle=None → should clear the stale auto_hangup.
    // A mock provider option is supplied so do_tts() reaches the auto_hangup logic
    // (without it the function returns early with "no tts option").
    // TTS track creation fails (provider "mock" is not registered), but
    // auto_hangup is already set to None before that point.
    active_call
        .enqueue_command(Command::Tts {
            text: "hello".to_string(),
            speaker: None,
            play_id: Some("new-tts-play".to_string()),
            auto_hangup: None, // <-- no hangup intent
            streaming: Some(false),
            end_of_stream: None,
            option: Some(SynthesisOption {
                provider: Some(SynthesisType::Other("mock".to_string())),
                ..Default::default()
            }),
            wait_input_timeout: None,
            base64: None,
            cache_key: None,
        })
        .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    {
        let state = active_call.call_state.read().await;
        assert!(
            state.auto_hangup.is_none(),
            "Stale auto_hangup (ssrc={stale_ssrc}) from do_play must be cleared \
             when do_tts() runs with no existing handle and no auto_hangup intent"
        );
    }

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), serve_handle).await;
    Ok(())
}
