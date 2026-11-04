//! End to end over a real `RtcTrack` (RTP mode, jitter buffer forced on):
//! inbound audio sent in bursts, with one packet lost and two swapped, must
//! come out in order, paced at the packet time, with the lost frame
//! concealed rather than skipped; and a sender going quiet after comfort
//! noise must not be concealed as loss.
use crate::event::create_event_sender;
use crate::media::AudioFrame;
use crate::media::Samples;
use crate::media::track::rtc::{RtcTrack, RtcTrackConfig};
use crate::media::track::{Track, TrackConfig};
use anyhow::Result;
use audio_codec::{CodecType, Encoder, pcma::PcmaEncoder};
use crate::media::track::rtc::TransportMode;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

const SSRC: u32 = 0x5566_7788;

/// One 20 ms PCMA packet of a continuous 200 Hz tone.
fn tone_packet(encoder: &mut PcmaEncoder, seq: u16) -> Vec<u8> {
    let pcm: Vec<i16> = (0..160)
        .map(|i| {
            let t = (seq as usize * 160 + i) as f32 / 8000.0;
            (8000.0 * (2.0 * std::f32::consts::PI * 200.0 * t).sin()) as i16
        })
        .collect();
    let mut pkt = vec![0x80, 8];
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&(seq as u32 * 160).to_be_bytes());
    pkt.extend_from_slice(&SSRC.to_be_bytes());
    pkt.extend_from_slice(&encoder.encode(&pcm));
    pkt
}

/// A comfort noise packet (RFC 3389): the sender goes quiet after it.
fn comfort_noise_packet(seq: u16) -> Vec<u8> {
    let mut pkt = vec![0x80, 13];
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&(seq as u32 * 160).to_be_bytes());
    pkt.extend_from_slice(&SSRC.to_be_bytes());
    // noise level, -70 dBov
    pkt.push(70);
    pkt
}

/// A started PCMA track with the jitter buffer on, the RTP port it listens
/// on, and the frames it emits.
async fn playout_track() -> Result<(
    RtcTrack,
    u16,
    tokio::sync::mpsc::UnboundedReceiver<AudioFrame>,
)> {
    let rtc_config = RtcTrackConfig {
        mode: TransportMode::Rtp,
        bind_ip: Some("127.0.0.1".to_string()),
        codecs: vec![CodecType::PCMA],
        preferred_codec: Some(CodecType::PCMA),
        jitter_buffer: Some(true),
        ..Default::default()
    };
    let mut track = RtcTrack::new(
        CancellationToken::new(),
        "playout-track".to_string(),
        TrackConfig {
            codec: CodecType::PCMA,
            ..Default::default()
        },
        rtc_config,
    )
    .with_ssrc(0x1122_3344);
    // concealment works on decoded audio, which production gets from its
    // processors
    track.processor_chain().force_decode = true;
    track.create().await?;

    let offer = "v=0\r\n\
         o=- 1 1 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio 45997 RTP/AVP 8\r\n\
         a=rtpmap:8 PCMA/8000\r\n"
        .to_string();
    let answer = track.handshake(offer, Some(Duration::from_secs(5))).await?;
    let port: u16 = answer
        .lines()
        .find_map(|l| {
            l.strip_prefix("m=audio ")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .expect("no m=audio port in answer");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    track.start(create_event_sender(), tx).await?;
    Ok((track, port, rx))
}

/// Collects `(arrival, sequence number or None for a concealed frame, rms)`
/// until no frame came for 500 ms.
fn collect(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<AudioFrame>,
) -> tokio::task::JoinHandle<Vec<(Instant, Option<u16>, f32)>> {
    tokio::spawn(async move {
        let mut out = vec![];
        while let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_millis(500), rx.recv()).await
        {
            let Samples::PCM { samples } = &frame.samples else {
                panic!("expected decoded audio, got {:?}", frame.samples);
            };
            let rms = (samples.iter().map(|s| (*s as f32).powi(2)).sum::<f32>()
                / samples.len() as f32)
                .sqrt();
            out.push((
                Instant::now(),
                frame.src_packet.as_ref().map(|p| p.sequence_number),
                rms,
            ));
        }
        out
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bursty_lossy_rtp_plays_out_paced_and_concealed() -> Result<()> {
    let (_track, port, rx) = playout_track().await?;
    let collector = collect(rx);

    // 30 packets in bursts of three every 60 ms; 10 lost, 12 before 11
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let remote = format!("127.0.0.1:{port}");
    let mut encoder = PcmaEncoder::new();
    let order: Vec<u16> = (0..30u16)
        .filter(|s| *s != 10)
        .map(|s| match s {
            11 => 12,
            12 => 11,
            s => s,
        })
        .collect();
    for burst in order.chunks(3) {
        for seq in burst {
            socket
                .send_to(&tone_packet(&mut encoder, *seq), &remote)
                .await?;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let out = collector.await?;

    let seqs: Vec<Option<u16>> = out.iter().map(|o| o.1).collect();
    let real: Vec<u16> = seqs.iter().flatten().copied().collect();
    let expected: Vec<u16> = (0..30).filter(|s| *s != 10).collect();
    assert_eq!(real, expected, "frames out of order or missing: {seqs:?}");
    // the lost frame is concealed where it belongs
    let at = seqs.iter().position(|s| *s == Some(9)).unwrap();
    assert_eq!(seqs[at + 1], None, "no concealment for 10: {seqs:?}");
    assert_eq!(seqs[at + 2], Some(11));
    assert!(out[at + 1].2 > 1000.0, "concealment is silent");
    // sent three at a time, played one per 20 ms: without the buffer each
    // burst arrives within a millisecond
    let bunched = out
        .windows(2)
        .filter(|w| w[1].0 - w[0].0 < Duration::from_millis(5))
        .count();
    assert!(
        bunched <= 3,
        "{bunched} of {} frames came out bunched, not paced",
        out.len()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender_going_quiet_after_comfort_noise_is_not_concealed() -> Result<()> {
    // concealed frames after the sender stops at `stop`, with or without
    // a comfort noise packet first
    async fn concealed_after_stop(comfort_noise: bool) -> Result<usize> {
        let (_track, port, rx) = playout_track().await?;
        let collector = collect(rx);
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let remote = format!("127.0.0.1:{port}");
        let mut encoder = PcmaEncoder::new();
        for seq in 0..10u16 {
            socket
                .send_to(&tone_packet(&mut encoder, seq), &remote)
                .await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if comfort_noise {
            socket.send_to(&comfort_noise_packet(10), &remote).await?;
        }
        let out = collector.await?;
        let real: Vec<u16> = out.iter().filter_map(|o| o.1).collect();
        assert_eq!(real, (0..10).collect::<Vec<_>>());
        Ok(out.iter().filter(|o| o.1.is_none()).count())
    }
    assert!(
        concealed_after_stop(false).await? > 0,
        "a stream just stopping is concealed (it may be late)"
    );
    assert_eq!(
        concealed_after_stop(true).await?,
        0,
        "silence after comfort noise was concealed as loss"
    );
    Ok(())
}
