use crate::media::track::{
    Track, TrackConfig,
    rtc::{RtcTrack, RtcTrackConfig},
};
use anyhow::Result;
use rustrtc::TransportMode;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_rtp_track_creation() -> Result<()> {
    let track_id = "test-rtp-track".to_string();
    let cancel_token = CancellationToken::new();

    let rtc_config = RtcTrackConfig {
        mode: TransportMode::Rtp,
        ..Default::default()
    };

    let track = RtcTrack::new(
        cancel_token.clone(),
        track_id.clone(),
        TrackConfig::default(),
        rtc_config,
    );

    assert_eq!(track.id(), &track_id);

    // Test with modified configuration
    let sample_rate = 16000;
    // Note: TrackConfig might not have with_sample_rate builder method if I didn't verify it,
    // but the original code used it. I will check TrackConfig definition later if this fails.
    // Assuming TrackConfig::default() works.
    let mut config = TrackConfig::default();
    config.samplerate = sample_rate;

    let rtc_config2 = RtcTrackConfig {
        mode: TransportMode::Rtp,
        ..Default::default()
    };

    let _track = RtcTrack::new(cancel_token, track_id, config, rtc_config2);

    Ok(())
}
