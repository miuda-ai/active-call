//! End-to-end verification of `mediaPass.useControlWs`.
//!
//! The call dials out over SIP (`/call/sip`) and carries the agent-side audio
//! over the *same* control websocket: no second websocket is opened.
//!
//! What is asserted with real SIP/RTP media:
//! - RTP arriving from the callee is delivered to the websocket client as raw
//!   PCM16LE binary frames, chunked by `packetSize` (no `[trackIndex]` header,
//!   even when `subscribe: true` is also set);
//! - PCM binary frames sent by the client are buffered and played into the SIP
//!   leg at `ptime` (20 ms), i.e. the callee receives paced RTP, not a burst;
//! - the SIP dialog is established normally (INVITE → 200 OK → ACK → RTP).

use active_call::{
    app::{AppState, AppStateBuilder},
    config::Config,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, UdpSocket};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::Level;

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct TestNode {
    http_port: u16,
}

async fn spawn_node() -> TestNode {
    let config = Config {
        addr: "127.0.0.1".to_string(),
        udp_port: 0,
        log_level: Some("info".to_string()),
        useragent: Some("ActiveCallTest".to_string()),
        media_cache_path: "./target/tmp_media".to_string(),
        ..Default::default()
    };
    let app: AppState = AppStateBuilder::new()
        .with_config(config)
        .build()
        .await
        .expect("failed to build app state");
    let sip_app = app.clone();
    let http_app = app.clone();
    tokio::spawn(async move {
        let _ = sip_app.serve().await;
    });
    let http_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let http_port = http_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            http_listener,
            active_call::handler::call_router().with_state(http_app),
        )
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    TestNode { http_port }
}

fn rtp_packet(seq: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(12 + payload.len());
    pkt.extend_from_slice(&[0x80, 0x00]); // V=2, PT=0 (PCMU)
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&timestamp.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(payload);
    pkt
}

fn sdp_media_endpoint(msg: &str) -> (String, u16) {
    let sdp = msg.split("\r\n\r\n").nth(1).unwrap_or("");
    let mut host = None;
    let mut port = None;
    for line in sdp.lines() {
        if let Some(v) = line.strip_prefix("c=IN IP4 ") {
            host = Some(v.trim().to_string());
        }
        if let Some(v) = line.strip_prefix("m=audio ") {
            port = v.split_whitespace().next().and_then(|p| p.parse().ok());
        }
    }
    (
        host.expect("SDP missing c= line"),
        port.expect("SDP missing m=audio port"),
    )
}

/// Answer the outgoing INVITE with PCMU and return the SIP orientation data.
async fn answer_invite(
    uas_sip: &UdpSocket,
    uas_media_port: u16,
    uas_port: u16,
) -> (String, std::net::SocketAddr) {
    let mut buf = [0u8; 8192];
    let (invite_msg, invite_src) = loop {
        let (n, src) = tokio::time::timeout(Duration::from_secs(10), uas_sip.recv_from(&mut buf))
            .await
            .expect("timeout waiting for INVITE")
            .expect("recv error");
        let msg = String::from_utf8_lossy(&buf[..n]).to_string();
        if msg.starts_with("INVITE") {
            break (msg, src);
        }
    };

    let mut via = String::new();
    let mut from = String::new();
    let mut call_id = String::new();
    let mut cseq = String::new();
    for line in invite_msg.lines() {
        for (prefix, slot) in [
            ("Via:", &mut via),
            ("From:", &mut from),
            ("Call-ID:", &mut call_id),
            ("CSeq:", &mut cseq),
        ] {
            if line.starts_with(prefix) && slot.is_empty() {
                *slot = line.to_string();
            }
        }
    }
    let to_tag = "uas-tag-1";
    let sdp = format!(
        "v=0\r\n\
         o=- 654321 1 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {uas_media_port} RTP/AVP 0\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=sendrecv\r\n"
    );
    let ok = format!(
        "SIP/2.0 200 OK\r\n\
         {via}\r\n\
         {from}\r\n\
         To: <sip:agent@127.0.0.1:{uas_port}>;tag={to_tag}\r\n\
         {call_id}\r\n\
         {cseq}\r\n\
         Contact: <sip:agent@127.0.0.1:{uas_port}>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\
         \r\n\
         {sdp}",
        len = sdp.len(),
    );
    uas_sip.send_to(ok.as_bytes(), invite_src).await.unwrap();
    (invite_msg, invite_src)
}

async fn wait_for_event(
    ws: &mut futures::stream::SplitStream<WsStream>,
    needle: &str,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if text.contains(needle) {
                    return true;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return false,
        }
    }
}

/// One 20 ms PCM16 frame of a constant-amplitude tone at 16 kHz.
fn pcm16_frame(value: i16) -> Bytes {
    let mut bytes = Vec::with_capacity(640);
    for _ in 0..320 {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Bytes::from(bytes)
}

#[tokio::test]
async fn sip_call_with_media_pass_over_control_ws() {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_test_writer()
        .try_init()
        .ok();

    let node = spawn_node().await;

    // Minimal raw-SIP UAS: answers the outgoing INVITE and exchanges RTP.
    let uas_sip = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let uas_media = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let uas_port = uas_sip.local_addr().unwrap().port();
    let uas_media_port = uas_media.local_addr().unwrap().port();

    let ws_url = format!(
        "ws://127.0.0.1:{}/call/sip?id=media-pass-control-ws-e2e",
        node.http_port
    );
    let (ws, _) = connect_async(ws_url).await.expect("WS connect failed");
    let (mut bot_sink, mut bot_stream) = ws.split();

    // `subscribe: true` is set on purpose: with useControlWs the control-ws
    // audio path must be the only binary source, so no subscribe-format
    // `[trackIndex][PCM]` frames may appear.
    bot_sink
        .send(Message::text(
            json!({
                "command": "invite",
                "option": {
                    "caller": "sip:bot@127.0.0.1",
                    "callee": format!("sip:agent@127.0.0.1:{uas_port}"),
                    "subscribe": true,
                    "mediaPass": {
                        "useControlWs": true,
                        "inputSampleRate": 16000,
                        "outputSampleRate": 16000,
                        "packetSize": 640,
                        "ptime": 20
                    }
                }
            })
            .to_string(),
        ))
        .await
        .expect("failed to send invite");

    let (invite_msg, _invite_src) = answer_invite(&uas_sip, uas_media_port, uas_port).await;
    assert!(
        wait_for_event(&mut bot_stream, "\"event\":\"answer\"", Duration::from_secs(10)).await,
        "bot never received the answer event"
    );
    let (host, port) = sdp_media_endpoint(&invite_msg);
    let server_media: std::net::SocketAddr = format!("{host}:{port}").parse().unwrap();

    // ── callee -> client: RTP in, raw PCM out ─────────────────────────────
    let payload = vec![0x77u8; 160]; // 20 ms PCMU
    let send_rtp = 75u16;
    for seq in 0..send_rtp {
        let pkt = rtp_packet(seq, seq as u32 * 160, 0x1234_5678, &payload);
        uas_media.send_to(&pkt, server_media).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut binary_sizes: Vec<usize> = Vec::new();
    let mut subscribe_frames = 0usize;
    let mut first_binary: Option<Vec<u8>> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || binary_sizes.len() >= 30 {
            break;
        }
        match tokio::time::timeout(remaining, bot_stream.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => {
                if data.len() == 641 && (data[0] == 0 || data[0] == 1) {
                    subscribe_frames += 1;
                } else {
                    if first_binary.is_none() {
                        first_binary = Some(data.to_vec());
                    }
                    binary_sizes.push(data.len());
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("websocket error: {e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    assert!(
        binary_sizes.len() >= 20,
        "expected >= 20 raw PCM frames on the control websocket, got {} \
         (sizes: {:?})",
        binary_sizes.len(),
        binary_sizes
    );
    assert_eq!(
        subscribe_frames, 0,
        "subscribe-format [trackIndex][PCM] frames leaked alongside useControlWs"
    );
    assert!(
        binary_sizes.iter().all(|len| *len == 640),
        "control-ws audio must be chunked to packetSize (640), got sizes {:?}",
        binary_sizes
    );
    // PCMU 0x77 decodes to a small nonzero level; the frames must carry audio.
    let pcm = first_binary.expect("at least one binary frame");
    assert!(
        pcm.chunks(2)
            .any(|s| i16::from_le_bytes([s[0], s[1]]) != 0),
        "first control-ws PCM frame is all zeros"
    );

    // ── client -> callee: PCM in, paced RTP out ──────────────────────────
    // Collect RTP continuously while feeding PCM: reading the socket only
    // afterwards would measure the kernel queue drain, not the send pacing.
    let (rtp_tx, mut rtp_rx) =
        tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let reader = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            match uas_media.recv_from(&mut buf).await {
                Ok((n, _)) => {
                    let payload = if n > 12 { buf[12..n].to_vec() } else { Vec::new() };
                    let _ = rtp_tx.send((Instant::now(), payload));
                }
                Err(_) => break,
            }
        }
    });

    for _ in 0..50 {
        bot_sink
            .send(Message::Binary(pcm16_frame(6000)))
            .await
            .expect("failed to send PCM to the control websocket");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    reader.abort();

    let mut rtp_times: Vec<Instant> = Vec::new();
    let mut payload_seen = Vec::new();
    while let Ok((at, payload)) = rtp_rx.try_recv() {
        rtp_times.push(at);
        payload_seen.extend_from_slice(&payload);
    }

    assert!(
        rtp_times.len() >= 20,
        "expected >= 20 RTP packets at the UAS after streaming PCM, got {}",
        rtp_times.len()
    );
    let span = rtp_times.last().unwrap().duration_since(rtp_times[0]);
    let avg_gap = span / (rtp_times.len().max(1) as u32);
    assert!(
        avg_gap >= Duration::from_millis(5) && avg_gap <= Duration::from_millis(60),
        "RTP from the control-ws PCM was not paced at ptime: {} packets over {:?} \
         (avg {:?})",
        rtp_times.len(),
        span,
        avg_gap
    );
    assert!(
        span >= Duration::from_millis(250),
        "RTP burst detected: {} packets arrived within {:?}",
        rtp_times.len(),
        span
    );
    // PCMU 0xFF is digital silence; a 6000-amplitude PCM input must not encode
    // to all-silence.
    assert!(
        !payload_seen.is_empty(),
        "no RTP payload captured from the UAS"
    );
    assert!(
        payload_seen.iter().any(|b| *b != 0xFF),
        "callee received only PCMU silence although PCM audio was streamed"
    );
}
