/// Test for SIP Hold/Resume functionality via reinvites
/// This tests the detection and handling of ON HOLD states from SDP in reinvites
use active_call::media::{
    TrackId,
    track::{
        Track, TrackConfig,
        rtc::{RtcTrack, RtcTrackConfig},
    },
};
use anyhow::Result;
use audio_codec::CodecType;
use rustrtc::TransportMode;
use tokio_util::sync::CancellationToken;
use tracing::{Level, info};

const TEST_ANSWER_ACTIVE: &str = r#"v=0
o=- 654321 2 IN IP4 127.0.0.1
s=-
t=0 0
a=group:BUNDLE 0
a=msid-semantic: WMS
m=audio 9 UDP/TLS/RTP/SAVPF 0
c=IN IP4 127.0.0.1
a=rtcp:9 IN IP4 127.0.0.1
a=ice-ufrag:resp
a=ice-pwd:resppassword123456789012
a=ice-options:trickle
a=fingerprint:sha-256 11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11:11
a=setup:active
a=mid:0
a=sendrecv
a=rtcp-mux
a=rtpmap:0 PCMU/8000
"#;

const TEST_ANSWER_HOLD_SENDONLY: &str = r#"v=0
o=- 654321 3 IN IP4 127.0.0.1
s=-
t=0 0
a=group:BUNDLE 0
a=msid-semantic: WMS
m=audio 9 UDP/TLS/RTP/SAVPF 0
c=IN IP4 127.0.0.1
a=rtcp:9 IN IP4 127.0.0.1
a=ice-ufrag:resp2
a=ice-pwd:resp2password12345678901
a=ice-options:trickle
a=fingerprint:sha-256 22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22:22
a=setup:active
a=mid:0
a=sendonly
a=rtcp-mux
a=rtpmap:0 PCMU/8000
"#;

const TEST_ANSWER_HOLD_INACTIVE: &str = r#"v=0
o=- 654321 4 IN IP4 127.0.0.1
s=-
t=0 0
a=group:BUNDLE 0
a=msid-semantic: WMS
m=audio 9 UDP/TLS/RTP/SAVPF 0
c=IN IP4 127.0.0.1
a=rtcp:9 IN IP4 127.0.0.1
a=ice-ufrag:resp3
a=ice-pwd:resp3password12345678901
a=ice-options:trickle
a=fingerprint:sha-256 33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33:33
a=setup:active
a=mid:0
a=inactive
a=rtcp-mux
a=rtpmap:0 PCMU/8000
"#;

const TEST_ANSWER_HOLD_ZERO_ADDR: &str = r#"v=0
o=- 654321 5 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 RTP/AVP 0
c=IN IP4 0.0.0.0
a=rtpmap:0 PCMU/8000
a=sendrecv
"#;

/// Test hold detection with sendonly attribute
#[tokio::test]
async fn test_hold_detection_sendonly() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    info!("Starting test_hold_detection_sendonly");

    // Test the hold detection function
    let is_hold =
        active_call::media::negotiate::detect_hold_state_from_sdp(TEST_ANSWER_HOLD_SENDONLY);
    assert!(is_hold, "Should detect sendonly as hold state");

    let is_active = active_call::media::negotiate::detect_hold_state_from_sdp(TEST_ANSWER_ACTIVE);
    assert!(!is_active, "Should detect sendrecv as active state");

    info!("✓ Hold detection with sendonly works correctly");
    Ok(())
}

/// Test hold detection with inactive attribute
#[tokio::test]
async fn test_hold_detection_inactive() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    info!("Starting test_hold_detection_inactive");

    let is_hold =
        active_call::media::negotiate::detect_hold_state_from_sdp(TEST_ANSWER_HOLD_INACTIVE);
    assert!(is_hold, "Should detect inactive as hold state");

    info!("✓ Hold detection with inactive works correctly");
    Ok(())
}

/// Test hold detection with 0.0.0.0 connection address
#[tokio::test]
async fn test_hold_detection_zero_address() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    info!("Starting test_hold_detection_zero_address");

    let is_hold =
        active_call::media::negotiate::detect_hold_state_from_sdp(TEST_ANSWER_HOLD_ZERO_ADDR);
    assert!(is_hold, "Should detect 0.0.0.0 address as hold state");

    info!("✓ Hold detection with 0.0.0.0 works correctly");
    Ok(())
}

/// Test complete flow: active -> hold (sendonly) -> resume (sendrecv)
#[tokio::test]
async fn test_complete_hold_resume_flow() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    info!("Starting test_complete_hold_resume_flow");

    // Create RTC track with RTP mode (SIP scenario)
    let track_config = TrackConfig {
        codec: CodecType::PCMU,
        samplerate: 8000,
        ..Default::default()
    };

    let rtc_config = RtcTrackConfig {
        mode: TransportMode::Rtp,
        preferred_codec: Some(CodecType::PCMU),
        codecs: vec![CodecType::PCMU, CodecType::PCMA],
        ..Default::default()
    };

    let track_id: TrackId = "test-hold-track".to_string();
    let cancel_token = CancellationToken::new();

    let mut track = RtcTrack::new(
        cancel_token.clone(),
        track_id.clone(),
        track_config.clone(),
        rtc_config,
    );

    // Create the peer connection and generate local offer
    track.create().await?;
    let local_offer = track.local_description().await?;
    info!("Generated local offer:\n{}", local_offer);

    // Step 1: Set initial active call (sendrecv)
    info!("Step 1: Setting initial active call SDP");
    track
        .update_remote_description(&TEST_ANSWER_ACTIVE.to_string())
        .await?;
    info!("✓ Initial active call established");

    // Step 2: Receive reinvite with hold (sendonly)
    info!("Step 2: Simulating reinvite with hold (sendonly)");
    let result = track
        .update_remote_description(&TEST_ANSWER_HOLD_SENDONLY.to_string())
        .await;

    match result {
        Ok(_) => {
            info!("✓ Successfully handled reinvite with hold state");
        }
        Err(e) => {
            panic!("✗ Failed to handle reinvite with hold: {}", e);
        }
    }

    // Step 3: Receive another reinvite to resume (sendrecv)
    info!("Step 3: Simulating reinvite to resume (sendrecv)");
    let result = track
        .update_remote_description(&TEST_ANSWER_ACTIVE.to_string())
        .await;

    match result {
        Ok(_) => {
            info!("✓ Successfully handled reinvite to resume");
        }
        Err(e) => {
            panic!("✗ Failed to handle reinvite to resume: {}", e);
        }
    }

    info!("✓ Complete hold/resume flow test passed");
    Ok(())
}

/// Test multiple hold/resume toggles
#[tokio::test]
async fn test_multiple_hold_toggles() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    info!("Starting test_multiple_hold_toggles");

    let track_config = TrackConfig {
        codec: CodecType::PCMU,
        samplerate: 8000,
        ..Default::default()
    };

    let rtc_config = RtcTrackConfig {
        mode: TransportMode::Rtp,
        preferred_codec: Some(CodecType::PCMU),
        codecs: vec![CodecType::PCMU],
        ..Default::default()
    };

    let track_id: TrackId = "test-toggle-track".to_string();
    let cancel_token = CancellationToken::new();

    let mut track = RtcTrack::new(
        cancel_token.clone(),
        track_id.clone(),
        track_config.clone(),
        rtc_config,
    );

    track.create().await?;
    let _offer = track.local_description().await?;

    // Initial active state
    track
        .update_remote_description(&TEST_ANSWER_ACTIVE.to_string())
        .await?;
    info!("✓ Initial state: active");

    // Toggle to hold (inactive)
    track
        .update_remote_description(&TEST_ANSWER_HOLD_INACTIVE.to_string())
        .await?;
    info!("✓ Toggle 1: held (inactive)");

    // Resume
    track
        .update_remote_description(&TEST_ANSWER_ACTIVE.to_string())
        .await?;
    info!("✓ Toggle 2: resumed");

    // Hold again (sendonly)
    track
        .update_remote_description(&TEST_ANSWER_HOLD_SENDONLY.to_string())
        .await?;
    info!("✓ Toggle 3: held (sendonly)");

    // Resume again
    track
        .update_remote_description(&TEST_ANSWER_ACTIVE.to_string())
        .await?;
    info!("✓ Toggle 4: resumed");

    info!("✓ Multiple hold/resume toggles test passed");
    Ok(())
}
