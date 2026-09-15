//! End-to-end regression for the native-samplerate recorder over a real
//! `RtcTrack` in RTP mode, mirroring the production wiring order for a SIP
//! leg: `RtcTrack::create()` (spawns media workers with a chain clone)
//! -> `MediaStream::update_track` (attaches the raw tap) -> RTP flows.
//!
//! Before the `Arc<RwLock>` raw_tap fix, the workers kept a stale `None` tap,
//! the recorder never received frames, never detected the native rate and the
//! WAV fell back to 16 kHz (with no audio data).
use crate::event::create_event_sender;
use crate::media::recorder::RecorderOption;
use crate::media::track::rtc::{RtcTrack, RtcTrackConfig};
use crate::media::track::TrackConfig;
use crate::media::{stream::MediaStreamBuilder, track::Track};
use anyhow::Result;
use audio_codec::CodecType;
use rustrtc::TransportMode;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::net::UdpSocket;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

/// Minimal WAV header check: returns (sample_rate, channels, data_size).
fn wav_header(path: &Path) -> Result<(u32, u16, u32)> {
    let bytes = std::fs::read(path)?;
    assert!(bytes.len() >= 44, "wav too small: {}", bytes.len());
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]);
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let data_size = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
    Ok((rate, channels, data_size))
}

/// Builds one 20ms PCMA RTP packet (PT=8, 160 alaw bytes).
fn rtp_packet(seq: u16, ts: u32, ssrc: u32) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(12 + 160);
    pkt.push(0x80);
    pkt.push(8); // PCMA
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&ts.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(&[0xD5u8; 160]); // alaw silence
    pkt
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_native_samplerate_recorder_over_rtc_rtp_track() -> Result<()> {
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("native_rtc_recording.wav");

    // MediaStream wired exactly like a call session: recorder configured in
    // native-samplerate mode, started by serve() before any track exists.
    let stream = Arc::new(
        MediaStreamBuilder::new(create_event_sender())
            .with_id("caller-track".to_string())
            .with_recorder_config(RecorderOption {
                recorder_file: file_path.to_string_lossy().to_string(),
                samplerate: 16000,
                ptime: 200,
                native_samplerate: Some(true),
                ..Default::default()
            })
            .build(),
    );
    {
        let stream = stream.clone();
        tokio::spawn(async move {
            stream.serve().await.ok();
        });
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Production order: create() BEFORE the recorder tap is attached ---
    let track_config = TrackConfig {
        codec: CodecType::PCMA,
        ..Default::default()
    };
    let mut rtc_config = RtcTrackConfig {
        mode: TransportMode::Rtp,
        bind_ip: Some("127.0.0.1".to_string()),
        codecs: vec![CodecType::PCMA, CodecType::TelephoneEvent],
        ..Default::default()
    };
    rtc_config.preferred_codec = Some(CodecType::PCMA);

    let mut track = RtcTrack::new(
        CancellationToken::new(),
        "caller-track".to_string(),
        track_config,
        rtc_config,
    )
    .with_ssrc(0x11223344);

    // Spawns the media workers that snapshot the ProcessorChain (raw tap not
    // attached yet).
    track.create().await?;

    let offer = "v=0\r\n\
         o=- 1 1 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio 45999 RTP/AVP 8 101\r\n\
         a=rtpmap:8 PCMA/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n"
        .to_string();

    let answer = track.handshake(offer, Some(Duration::from_secs(5))).await?;
    let track_port: u16 = answer
        .lines()
        .find_map(|l| l.strip_prefix("m=audio ").map(|rest| {
            rest.split_whitespace()
                .next()
                .and_then(|p| p.parse::<u16>().ok())
        }).flatten())
        .ok_or_else(|| anyhow::anyhow!("no m=audio port in answer:\n{answer}"))?;
    assert!(track_port > 0, "track did not bind an RTP port");

    // Now the tap gets attached - the workers were already spawned above.
    let boxed = Box::new(track) as Box<dyn Track>;
    stream.update_track(boxed, None).await;

    // --- Stream PCMA RTP from the "gateway" ---
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let remote = format!("127.0.0.1:{track_port}");
    let packets = 60; // 60 * 20ms = 1.2s of audio
    for i in 0..packets {
        socket
            .send_to(&rtp_packet(i as u16, (i as u32) * 160, 0x11223344), &remote)
            .await?;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Give the recorder a moment, then stop: cancels the recorder token so it
    // flushes and rewrites the WAV header with the detected native rate.
    tokio::time::sleep(Duration::from_millis(300)).await;
    stream.stop(None, None);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        file_path.exists(),
        "recording file never created - the recorder received no frames \
         (raw tap not visible to the RtcTrack workers)"
    );
    let (rate, channels, data_size) = wav_header(&file_path)?;
    assert_eq!(rate, 8000, "native rate from the PCMA caller leg expected");
    assert_eq!(channels, 2, "native recorder writes stereo wav");
    assert!(
        data_size >= 2 * 2 * 8000, // at least 1s of 8kHz stereo 16-bit
        "recording has no/short audio data: {data_size}"
    );
    let total = std::fs::metadata(&file_path)?.len() as u32;
    assert_eq!(total, 44 + data_size, "wav header/data inconsistent");

    Ok(())
}
