//! Memory-leak regression tests for the lock-free call state refactor.
//!
//! Strategy:
//! 1. Run many full call lifecycles (create -> serve -> commands/events ->
//!    cancel -> drop) and assert:
//!    - every call produced exactly one call record (the `Drop` path that used
//!      to lose records on `try_read` failure always runs),
//!    - the `active_calls` registry and pending-dialog map drain to empty,
//!    - the only remaining strong reference to each call is the test's own
//!      (i.e. nothing — dialog tasks, media tasks, spawned refer work — keeps
//!      the call alive after `serve` completes),
//!    - process RSS does not grow meaningfully with more cycles.
//!
//! RSS is read via `ps` (works on macOS/Linux); allocator retention makes a
//! zero-growth assertion impossible, so thresholds are generous but still
//! catch per-call leaks (a leaked MediaStream alone would add ~1 MB/cycle).

use active_call::app::AppStateBuilder;
use active_call::call::active_call::CallSpec;
use active_call::call::{ActiveCall, ActiveCallType};
use active_call::config::Config;
use active_call::event::SessionEvent;
use active_call::media::engine::StreamEngine;
use active_call::media::track::TrackConfig;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Resident set size in bytes of the current process.
fn rss_bytes() -> u64 {
    let pid = std::process::id();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("failed to run ps");
    let kb: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("ps returned non-numeric rss");
    kb * 1024
}

fn test_config() -> Config {
    let mut config = Config::default();
    config.udp_port = 0; // no SIP listener
    config.media_cache_path = "./target/tmp_leaktest".to_string();
    config
}

/// One full lifecycle: create the call, start the actor, exchange some
/// commands/events, register it in the guard registry, then tear down and
/// drop every reference.
async fn run_call_cycle(
    app_state: &active_call::app::AppState,
    record_rx: &mut mpsc::UnboundedReceiver<active_call::callrecord::CallRecord>,
    seed_events: usize,
) -> Result<()> {
    let cancel_token = CancellationToken::new();
    let call = Arc::new(ActiveCall::new(CallSpec {
        call_type: ActiveCallType::WebSocket,
        cancel_token: cancel_token.clone(),
        session_id: format!("leak-{}", uuid::Uuid::new_v4()),
        invitation: app_state.invitation.clone(),
        app_state: app_state.clone(),
        track_config: TrackConfig::default(),
        audio_receiver: None,
        dump_events: false,
        server_side_track_id: None,
        extras: None,
    }));

    // Register in the global registry like real handlers do.
    let _guard = active_call::call::active_call::ActiveCallGuard::new(call.clone());

    let receiver = call.new_receiver();
    let serve_handle = tokio::spawn({
        let call = call.clone();
        async move { call.serve(receiver).await }
    });

    // Drive the actor: one command echo + a few session events per call.
    call.enqueue_command(active_call::call::Command::Custom {
        sender: Some("leak-test".to_string()),
        data: serde_json::json!({}),
    })
    .await?;
    for i in 0..seed_events {
        let _ = call.event_sender.send(SessionEvent::Speaking {
            track_id: call.server_side_track_id.clone(),
            timestamp: active_call::media::get_timestamp(),
            start_time: i as u64,
            is_filler: None,
            confidence: None,
            refer: None,
        });
    }
    // Let the actor process them.
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Tear down.
    cancel_token.cancel();
    tokio::time::timeout(Duration::from_secs(10), serve_handle)
        .await
        .expect("serve did not finish")??;

    // After serve() completed, the spawned task's references (actor `me`,
    // dialog loops, media tasks) must be released. JoinHandle completion can
    // briefly precede task-local drops, so poll until only the three expected
    // references remain: ours, the registry guard's, and the registry map's.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while Arc::strong_count(&call) > 3 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        Arc::strong_count(&call),
        3,
        "leaked ActiveCall references after serve() finished"
    );
    // The call record is emitted from `Drop for ActiveCall`, so release our
    // strong reference (and the registry guard) before waiting for it.
    drop(_guard);
    drop(call);

    // Drop path must emit the call record exactly once per call.
    let record = tokio::time::timeout(Duration::from_secs(5), record_rx.recv())
        .await
        .expect("timed out waiting for call record")
        .expect("call record channel closed: Drop did not run");
    assert!(
        !record.call_id.is_empty(),
        "call record has no call id (snapshot was empty)"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_call_lifecycle_no_leak() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();

    let (record_tx, mut record_rx) = mpsc::unbounded_channel();
    let app_state = AppStateBuilder::new()
        .with_config(test_config())
        .with_stream_engine(Arc::new(StreamEngine::default()))
        .with_callrecord_sender(record_tx)
        .build()
        .await?;

    const WARMUP: usize = 50;
    const MEASURED: usize = 300;

    // Warmup: page in code paths, size allocator arenas.
    // (each cycle consumes its own call record, proving the Drop path runs)
    for _ in 0..WARMUP {
        run_call_cycle(&app_state, &mut record_rx, 4).await?;
    }

    let rss_before = rss_bytes();

    for i in 0..MEASURED {
        run_call_cycle(&app_state, &mut record_rx, 4).await?;
        if i % 100 == 0 {
            info!(cycle = i, rss = rss_bytes(), "leak test progress");
        }
    }

    let rss_after = rss_bytes();
    let growth = rss_after.saturating_sub(rss_before);
    let per_call = growth / MEASURED as u64;

    // Registry and pending dialogs must be drained.
    assert!(
        app_state.active_calls.lock().unwrap().is_empty(),
        "active_calls registry leaked entries"
    );
    assert!(
        app_state
            .invitation
            .pending_dialogs
            .lock()
            .unwrap()
            .is_empty(),
        "pending_dialogs leaked entries"
    );

    info!(
        rss_before = rss_before,
        rss_after = rss_after,
        growth = growth,
        per_call = per_call,
        "leak test summary"
    );

    // Thresholds: generous enough for allocator retention on macOS/Linux,
    // small enough to fail if a per-call MediaStream (~1 MB) leaks.
    assert!(
        per_call < 64 * 1024,
        "RSS grew by {} bytes/cycle — likely per-call leak",
        per_call
    );
    assert!(
        growth < 64 * 1024 * 1024,
        "total RSS growth {} bytes is too high",
        growth
    );

    Ok(())
}

/// Refer-leg bookkeeping: after the leg is replaced/cleared, no extra
/// references to the LegShared (progress/extras ArcSwaps) survive.
#[tokio::test]
async fn test_refer_leg_references_released() -> Result<()> {
    let app_state = AppStateBuilder::new()
        .with_config(test_config())
        .with_stream_engine(Arc::new(StreamEngine::default()))
        .build()
        .await?;

    let call = Arc::new(ActiveCall::new(CallSpec {
        call_type: ActiveCallType::Sip,
        cancel_token: CancellationToken::new(),
        session_id: "refer-leak".to_string(),
        invitation: app_state.invitation.clone(),
        app_state: app_state.clone(),
        track_config: TrackConfig::default(),
        audio_receiver: None,
        dump_events: false,
        server_side_track_id: None,
        extras: None,
    }));

    let leg = active_call::call::state::LegShared::new(7, true, Default::default());
    // `leg` itself holds one reference to the progress ArcSwap.
    assert_eq!(Arc::strong_count(&leg.progress), 1);

    call.set_refer_leg(Some(leg.clone()));
    // After storing: local `leg` + the clone inside the ArcSwap = 2.
    assert_eq!(
        Arc::strong_count(&leg.progress),
        2,
        "unexpected reference count while refer leg is set"
    );

    // Clearing the slot must release the swap's reference.
    call.set_refer_leg(None);
    assert_eq!(
        Arc::strong_count(&leg.progress),
        1,
        "ArcSwap refer_leg kept a reference after clear"
    );
    drop(leg);
    Ok(())
}
