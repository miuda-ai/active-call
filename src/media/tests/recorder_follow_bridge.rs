//! `RecorderOption::follow_bridge`: a recording handed over to the bridged
//! peer when the recorded session ends.
use crate::media::{
    AudioFrame, Samples,
    recorder::RecorderOption,
    stream::{MediaStream, MediaStreamBuilder},
    track::{Track, TrackConfig, forwarding::ForwardingTrack},
};
use anyhow::Result;
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tempfile::tempdir;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const OP_VOICE: i16 = 1000;
const CLIENT_VOICE: i16 = -1000;
const CLIENT_TTS: i16 = 500;

/// A track whose audio is pushed in by the test. Like a real track's, its
/// frames run through its processor chain (the own recorder's tap) and then
/// into the stream (where adopted recordings are fed).
fn feed_track(id: &str) -> (Box<dyn Track>, mpsc::Sender<AudioFrame>) {
    let (inbound_tx, inbound_rx) = mpsc::channel(100);
    let (peer_tx, _) = mpsc::channel(1);
    let track = ForwardingTrack::new(
        id.to_string(),
        "none".to_string(),
        peer_tx,
        inbound_rx,
        TrackConfig::default(),
        CancellationToken::new(),
        rand::random::<u32>(),
        Arc::new(AtomicBool::new(false)),
    );
    (Box::new(track), inbound_tx)
}

async fn feed(track: &mpsc::Sender<AudioFrame>, value: i16, rate: u32, frames: usize) {
    for _ in 0..frames {
        let frame = AudioFrame {
            samples: Samples::PCM {
                samples: vec![value; (rate / 50) as usize], // 20ms
            },
            sample_rate: rate,
            channels: 1,
            ..Default::default()
        };
        track.send(frame).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn serve(stream: &Arc<MediaStream>) {
    let stream = stream.clone();
    tokio::spawn(async move { stream.serve().await });
}

fn bridge(a: &Arc<MediaStream>, b: &Arc<MediaStream>) {
    a.set_bridge_peer(Some(b));
    b.set_bridge_peer(Some(a));
}

/// Hang up the stream (as the call ending does) and run its cleanup in the
/// background; the handle completes once the recording is finalized.
fn hang_up(stream: &Arc<MediaStream>) -> JoinHandle<()> {
    stream.cancel_token.cancel();
    let stream = stream.clone();
    tokio::spawn(async move {
        stream.cleanup().await.unwrap();
    })
}

struct Call {
    op: Arc<MediaStream>,
    client: Arc<MediaStream>,
    op_leg: mpsc::Sender<AudioFrame>,
    client_leg: mpsc::Sender<AudioFrame>,
    client_tts: mpsc::Sender<AudioFrame>,
}

/// An operator session recording itself, and a client session without a
/// recorder; both bridged to each other.
async fn bridged_call(file: &Path, follow_bridge: Option<bool>, native: bool) -> Call {
    bridged_call_with(file, follow_bridge, native, false).await
}

async fn bridged_call_with(
    file: &Path,
    follow_bridge: Option<bool>,
    native: bool,
    swap_channels: bool,
) -> Call {
    let op = Arc::new(
        MediaStreamBuilder::new(crate::event::create_event_sender())
            .with_id("op".to_string())
            .with_recorder_config(RecorderOption {
                recorder_file: file.to_string_lossy().to_string(),
                native_samplerate: Some(native),
                follow_bridge,
                swap_channels: Some(swap_channels),
                ..Default::default()
            })
            .build(),
    );
    let client = Arc::new(
        MediaStreamBuilder::new(crate::event::create_event_sender())
            .with_id("cl".to_string())
            .build(),
    );
    let (track, op_leg) = feed_track("op");
    op.update_track(track, None).await;
    let (track, client_leg) = feed_track("cl");
    client.update_track(track, None).await;
    let (track, client_tts) = feed_track("server-side-track");
    client.update_track(track, None).await;
    serve(&op);
    serve(&client);
    bridge(&op, &client);
    tokio::time::sleep(Duration::from_millis(50)).await;
    Call {
        op,
        client,
        op_leg,
        client_leg,
        client_tts,
    }
}

async fn finished(handle: &mut JoinHandle<()>, within: Duration) -> bool {
    tokio::time::timeout(within, handle).await.is_ok()
}

fn read_channels(file: &Path) -> Result<(Vec<i16>, Vec<i16>)> {
    let mut reader = hound::WavReader::open(file)?;
    assert_eq!(reader.spec().channels, 2);
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
    let left = samples.iter().step_by(2).copied().collect();
    let right = samples.iter().skip(1).step_by(2).copied().collect();
    Ok((left, right))
}

async fn assert_follows_bridge(native: bool, swap_channels: bool) -> Result<()> {
    let rate = if native { 8000 } else { 16000 };
    let dir = tempdir()?;
    let file = dir.path().join("follow.wav");
    let call = bridged_call_with(&file, Some(true), native, swap_channels).await;

    feed(&call.op_leg, OP_VOICE, rate, 10).await;
    let mut op_done = hang_up(&call.op);
    assert!(
        !finished(&mut op_done, Duration::from_millis(300)).await,
        "operator cleanup must wait for the handed-over recording"
    );

    feed(&call.client_leg, CLIENT_VOICE, rate, 10).await;
    feed(&call.client_tts, CLIENT_TTS, rate, 10).await;
    hang_up(&call.client).await?;
    assert!(
        finished(&mut op_done, Duration::from_secs(5)).await,
        "recording must end with the peer"
    );

    let (left, right) = read_channels(&file)?;
    let (own, other) = if swap_channels {
        (right, left)
    } else {
        (left, right)
    };
    // The client's TTS takes the operator's place on the own channel.
    assert!(own.contains(&OP_VOICE) && own.contains(&CLIENT_TTS));
    assert!(!own.contains(&CLIENT_VOICE));
    assert!(other.contains(&CLIENT_VOICE));
    assert!(!other.contains(&OP_VOICE) && !other.contains(&CLIENT_TTS));
    Ok(())
}

#[tokio::test]
async fn test_recording_follows_bridge_after_hangup() -> Result<()> {
    assert_follows_bridge(false, false).await
}

#[tokio::test]
async fn test_recording_follows_bridge_after_hangup_native() -> Result<()> {
    assert_follows_bridge(true, false).await
}

/// With swapped channels the client stays on the left throughout, and the
/// operator, then the client's TTS, on the right.
#[tokio::test]
async fn test_recording_follows_bridge_with_swapped_channels() -> Result<()> {
    assert_follows_bridge(false, true).await?;
    assert_follows_bridge(true, true).await
}

#[tokio::test]
async fn test_recording_stops_on_hangup_without_follow_bridge() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("no_follow.wav");
    let call = bridged_call(&file, None, false).await;

    feed(&call.op_leg, OP_VOICE, 16000, 10).await;
    let mut op_done = hang_up(&call.op);
    assert!(finished(&mut op_done, Duration::from_secs(2)).await);

    feed(&call.client_leg, CLIENT_VOICE, 16000, 5).await;
    let (left, right) = read_channels(&file)?;
    assert!(left.contains(&OP_VOICE));
    assert!(!right.contains(&CLIENT_VOICE));
    Ok(())
}

#[tokio::test]
async fn test_no_handover_when_peer_bridged_elsewhere() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("rebridged.wav");
    let call = bridged_call(&file, Some(true), false).await;
    // e.g. the client was transferred to another operator before this one
    // hung up.
    let other = Arc::new(MediaStreamBuilder::new(crate::event::create_event_sender()).build());
    call.client.set_bridge_peer(Some(&other));

    feed(&call.op_leg, OP_VOICE, 16000, 5).await;
    let mut op_done = hang_up(&call.op);
    assert!(finished(&mut op_done, Duration::from_secs(2)).await);
    Ok(())
}

#[tokio::test]
async fn test_new_bridge_ends_adopted_recording() -> Result<()> {
    let dir = tempdir()?;
    let file = dir.path().join("adopted_rebridge.wav");
    let call = bridged_call(&file, Some(true), false).await;

    feed(&call.op_leg, OP_VOICE, 16000, 5).await;
    let mut op_done = hang_up(&call.op);
    feed(&call.client_leg, CLIENT_VOICE, 16000, 5).await;
    assert!(!finished(&mut op_done, Duration::from_millis(100)).await);

    let other = Arc::new(MediaStreamBuilder::new(crate::event::create_event_sender()).build());
    call.client.set_bridge_peer(Some(&other));
    assert!(finished(&mut op_done, Duration::from_secs(2)).await);

    // Audio after the new bridge stays out of the finished recording.
    let (_, right_before) = read_channels(&file)?;
    feed(&call.client_leg, CLIENT_VOICE, 16000, 5).await;
    let (_, right_after) = read_channels(&file)?;
    assert_eq!(right_before.len(), right_after.len());
    Ok(())
}
