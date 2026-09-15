//! Performance tests for the lock-free call state.
//!
//! These are regression-oriented benchmarks with generous assertions: they
//! print ns/op numbers for humans and fail only if something regresses by an
//! order of magnitude (e.g. accidentally reintroducing a blocking lock on a
//! hot path).

use active_call::app::AppStateBuilder;
use active_call::call::active_call::CallSpec;
use active_call::call::state::{CallProgress, LegShared};
use active_call::call::{ActiveCall, ActiveCallType, Command};
use active_call::config::Config;
use active_call::event::SessionEvent;
use active_call::media::engine::StreamEngine;
use active_call::media::track::TrackConfig;
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::info;

fn test_config() -> Config {
    let mut config = Config::default();
    config.udp_port = 0;
    config.media_cache_path = "./target/tmp_perftest".to_string();
    config
}

/// Snapshot reads: ArcSwap load vs the old tokio RwLock read.
/// ArcSwap must be at least as fast (in practice it is 5-20x faster since
/// it is a single atomic load).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bench_progress_snapshot_arcswap_vs_rwlock() -> Result<()> {
    const N: u32 = 200_000;

    let leg = LegShared::new(1, false, CallProgress::default());
    let rw: Arc<RwLock<CallProgress>> = Arc::new(RwLock::new(CallProgress::default()));

    // ArcSwap
    let t = Instant::now();
    let mut sink = 0u64;
    for _ in 0..N {
        let p = leg.progress.load_full();
        sink += p.last_status_code as u64;
    }
    let arcswap_ns = t.elapsed().as_nanos() as f64 / N as f64;

    // RwLock (async read, like the old `call_state.read().await`)
    let t = Instant::now();
    for _ in 0..N {
        let g = rw.read().await;
        sink += g.last_status_code as u64;
    }
    let rwlock_ns = t.elapsed().as_nanos() as f64 / N as f64;

    info!(
        arcswap_ns_per_op = arcswap_ns,
        rwlock_ns_per_op = rwlock_ns,
        speedup = rwlock_ns / arcswap_ns.max(1.0),
        sink,
        "snapshot bench"
    );
    assert!(sink <= N as u64 * 2, "keep sink alive");

    assert!(
        arcswap_ns < rwlock_ns,
        "ArcSwap snapshot ({:.1}ns) should beat RwLock read ({:.1}ns)",
        arcswap_ns,
        rwlock_ns
    );
    assert!(
        arcswap_ns < 200.0,
        "ArcSwap snapshot regressed: {:.1}ns/op",
        arcswap_ns
    );
    Ok(())
}

/// rcu mutations (the write side used at dialog transition points).
#[tokio::test]
async fn bench_progress_rcu_writes() -> Result<()> {
    const N: u32 = 100_000;
    let leg = LegShared::new(1, false, CallProgress::default());

    let t = Instant::now();
    for i in 0..N {
        leg.update_progress(|p| p.on_early((180 + (i % 20)) as u16));
    }
    let ns = t.elapsed().as_nanos() as f64 / N as f64;
    info!(rcu_ns_per_op = ns, "progress rcu bench");
    assert!(ns < 5_000.0, "progress rcu regressed: {:.1}ns/op", ns);
    Ok(())
}

/// extras set_var (playbook variable writes, previously a write lock).
#[tokio::test]
async fn bench_extras_set_var() -> Result<()> {
    const N: u32 = 100_000;
    let leg = LegShared::new(1, false, CallProgress::default());

    let t = Instant::now();
    for i in 0..N {
        leg.set_extra(
            &format!("var_{}", i % 64),
            serde_json::Value::Number(i.into()),
        );
    }
    let ns = t.elapsed().as_nanos() as f64 / N as f64;
    info!(set_var_ns_per_op = ns, "extras set_var bench");
    // Debug builds are unoptimized; the threshold still catches an
    // order-of-magnitude regression (e.g. a lock plus full re-render).
    assert!(ns < 20_000.0, "extras set_var regressed: {:.1}ns/op", ns);
    Ok(())
}

/// End-to-end actor round trip: enqueue a Custom command, measure until the
/// echoed Custom event comes back on the broadcast channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_actor_command_round_trip() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();

    let app_state = AppStateBuilder::new()
        .with_config(test_config())
        .with_stream_engine(Arc::new(StreamEngine::default()))
        .build()
        .await?;

    let cancel_token = CancellationToken::new();
    let call = Arc::new(ActiveCall::new(CallSpec {
        call_type: ActiveCallType::WebSocket,
        cancel_token: cancel_token.clone(),
        session_id: "perf-actor".to_string(),
        invitation: app_state.invitation.clone(),
        app_state: app_state.clone(),
        track_config: TrackConfig::default(),
        audio_receiver: None,
        dump_events: false,
        server_side_track_id: None,
        extras: None,
    }));

    let mut event_receiver = call.event_sender.subscribe();
    let receiver = call.new_receiver();
    let serve_handle = tokio::spawn({
        let call = call.clone();
        async move { call.serve(receiver).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    const N: u32 = 2_000;
    let mut latencies_us: Vec<f64> = Vec::with_capacity(N as usize);
    let mut seq: u64 = 0;

    for _ in 0..N {
        seq += 1;
        let sent = Instant::now();
        call.enqueue_command(Command::Custom {
            sender: Some("perf".to_string()),
            data: serde_json::json!({ "seq": seq }),
        })
        .await?;

        // Wait for our echo (skip events from other sources).
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), event_receiver.recv())
                .await
                .expect("timed out waiting for echo")?;
            if let SessionEvent::Custom { data, .. } = event {
                if data.get("seq").and_then(|v| v.as_u64()) == Some(seq) {
                    break;
                }
            }
        }
        latencies_us.push(sent.elapsed().as_nanos() as f64 / 1_000.0);
    }

    latencies_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = latencies_us.iter().sum::<f64>() / latencies_us.len() as f64;
    let p50 = latencies_us[latencies_us.len() / 2];
    let p99 = latencies_us[(latencies_us.len() * 99) / 100];
    let max = latencies_us[latencies_us.len() - 1];

    info!(
        n = N,
        avg_us = avg,
        p50_us = p50,
        p99_us = p99,
        max_us = max,
        "actor command round trip"
    );

    // Generous bounds for CI machines: the actor should handle a command
    // round trip in well under a millisecond on average.
    assert!(avg < 500.0, "avg round trip too slow: {:.1}us", avg);
    assert!(p99 < 5_000.0, "p99 round trip too slow: {:.1}us", p99);

    cancel_token.cancel();
    tokio::time::timeout(Duration::from_secs(10), serve_handle).await???;
    Ok(())
}

/// Call setup/teardown throughput (create + serve + cancel + drop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_call_setup_teardown_rate() -> Result<()> {
    let app_state = AppStateBuilder::new()
        .with_config(test_config())
        .with_stream_engine(Arc::new(StreamEngine::default()))
        .build()
        .await?;

    const N: u32 = 200;

    // Warmup
    for _ in 0..20 {
        one_cycle(&app_state).await?;
    }

    let t = Instant::now();
    for _ in 0..N {
        one_cycle(&app_state).await?;
    }
    let elapsed = t.elapsed();
    let per_cycle = elapsed.as_nanos() as f64 / N as f64;
    let rate = 1e9 / per_cycle;
    info!(
        n = N,
        elapsed_ms = elapsed.as_millis() as u64,
        per_cycle_us = per_cycle / 1_000.0,
        cycles_per_sec = rate,
        "call setup/teardown rate"
    );

    // A cycle involves spawning the actor and the media-stream task; anything
    // above 100/s is healthy (media task spawn dominates).
    assert!(rate > 100.0, "setup/teardown rate too low: {:.1}/s", rate);
    Ok(())
}

async fn one_cycle(app_state: &active_call::app::AppState) -> Result<()> {
    let cancel_token = CancellationToken::new();
    let call = Arc::new(ActiveCall::new(CallSpec {
        call_type: ActiveCallType::WebSocket,
        cancel_token: cancel_token.clone(),
        session_id: format!("perf-{}", uuid::Uuid::new_v4()),
        invitation: app_state.invitation.clone(),
        app_state: app_state.clone(),
        track_config: TrackConfig::default(),
        audio_receiver: None,
        dump_events: false,
        server_side_track_id: None,
        extras: None,
    }));
    let receiver = call.new_receiver();
    let handle = tokio::spawn({
        let call = call.clone();
        async move { call.serve(receiver).await }
    });
    cancel_token.cancel();
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("serve hung")??;
    drop(call);
    Ok(())
}
