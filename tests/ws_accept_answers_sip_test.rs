//! Regression tests for the production failure seen with zapbx + VOS3000:
//!
//! An inbound SIP INVITE is handled by attaching a WebSocket bot client to
//! `/call?id=<dialogId>` (ActiveCallType::WebSocket) and issuing an `accept`
//! command. The underlying SIP dialog must then be answered with 200 OK and
//! carry bidirectional media.
//!
//! Before the fix, `do_accept` never answered the pending SIP dialog for
//! non-SIP call types: the transaction stayed in `Trying` forever and the
//! carrier eventually CANCELled the call (production: VOS3000 20s timeout,
//! 487 Request Terminated). The refer-connected agent then heard dead
//! silence because no customer media ever existed.

use active_call::{
    app::{AppState, AppStateBuilder},
    config::{Config, InviteHandlerConfig},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::{Level, info};

#[derive(Debug, Deserialize)]
struct WebhookPayload {
    #[serde(rename = "dialogId")]
    dialog_id: String,
    #[serde(rename = "sipCallId")]
    sip_call_id: String,
    event: String,
}

struct TestNode {
    http_port: u16,
    sip_port: u16,
    webhook_rx: mpsc::Receiver<WebhookPayload>,
}

async fn spawn_node(sip_port: u16, codecs: Vec<String>) -> TestNode {
    // Webhook server that reports the invite payload back to the test.
    let (webhook_tx, webhook_rx) = mpsc::channel::<WebhookPayload>(1);
    let webhook = axum::Router::new().route(
        "/mock-handler",
        axum::routing::post(
            move |axum::Json(payload): axum::Json<WebhookPayload>| async move {
                if payload.event == "invite" {
                    let _ = webhook_tx.try_send(payload);
                }
                axum::Json(serde_json::json!({"status": "ok"}))
            },
        ),
    );
    let webhook_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let webhook_port = webhook_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(webhook_listener, webhook).await.unwrap();
    });

    let port_key = (sip_port % 1000) as u16;
    let config = Config {
        addr: "127.0.0.1".to_string(),
        udp_port: sip_port,
        log_level: Some("debug".to_string()),
        useragent: Some("ActiveCallTest".to_string()),
        handler: Some(InviteHandlerConfig::Webhook {
            url: Some(format!("http://127.0.0.1:{webhook_port}/mock-handler")),
            method: Some("POST".to_string()),
            headers: None,
            urls: None,
        }),
        accept_timeout: Some("10s".to_string()),
        rtp_start_port: Some(31000 + port_key * 20),
        rtp_end_port: Some(31020 + port_key * 20),
        media_cache_path: "./target/tmp_media".to_string(),
        codecs: if codecs.is_empty() {
            None
        } else {
            Some(codecs)
        },
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

    TestNode {
        http_port,
        sip_port,
        webhook_rx,
    }
}

/// Minimal raw-SIP UAC: sends one INVITE and collects responses.
struct SipUac {
    socket: UdpSocket,
    server: std::net::SocketAddr,
}

impl SipUac {
    async fn new(server: std::net::SocketAddr, media_port: u16) -> Self {
        let socket = UdpSocket::bind(("127.0.0.1", media_port)).await.unwrap();
        Self { socket, server }
    }

    fn invite(&self, call_id: &str, from_tag: &str, branch: &str, sdp: &str) -> String {
        format!(
            "INVITE sip:bot@{server} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:{media_port};branch={branch};rport\r\n\
             From: <sip:caller@127.0.0.1>;tag={from_tag}\r\n\
             To: <sip:bot@{server}>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:caller@127.0.0.1:{media_port}>\r\n\
             Max-Forwards: 70\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {len}\r\n\
             \r\n\
             {sdp}",
            server = self.server,
            media_port = self.socket.local_addr().unwrap().port(),
            len = sdp.len(),
        )
    }

    /// CANCEL the INVITE (same branch/Call-ID/CSeq number, method CANCEL).
    fn cancel(&self, call_id: &str, from_tag: &str, branch: &str) -> String {
        format!(
            "CANCEL sip:bot@{server} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:{media_port};branch={branch};rport\r\n\
             From: <sip:caller@127.0.0.1>;tag={from_tag}\r\n\
             To: <sip:bot@{server}>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 CANCEL\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\
             \r\n",
            server = self.server,
            media_port = self.socket.local_addr().unwrap().port(),
        )
    }

    /// Wait for a specific status code; returns (matched, matched-message,
    /// all status lines seen).
    async fn wait_for_status(
        &self,
        needle: &str,
        timeout: Duration,
    ) -> (bool, Option<String>, Vec<String>) {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return (false, None, seen);
            }
            match tokio::time::timeout(remaining, self.socket.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
                    let status_line = msg.lines().next().unwrap_or("").to_string();
                    info!(%status_line, "UAC received response");
                    seen.push(status_line);
                    if msg.contains(needle) {
                        return (true, Some(msg), seen);
                    }
                }
                _ => return (false, None, seen),
            }
        }
    }

    /// Extract (host, port) of the audio media from a 200 OK.
    fn answer_media_endpoint(ok_msg: &str) -> (String, u16) {
        let sdp = ok_msg.split("\r\n\r\n").nth(1).unwrap_or("");
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
            host.expect("answer SDP missing c= line"),
            port.expect("answer SDP missing m=audio port"),
        )
    }

    /// Extract the `tag=` parameter from a To/From header line.
    fn header_tag(msg: &str, header: &str) -> Option<String> {
        msg.lines()
            .find(|l| l.starts_with(header))
            .and_then(|l| l.split("tag=").nth(1))
            .map(|t| t.trim().trim_end_matches(';').to_string())
    }

    /// Build an ACK for the answered INVITE (confirms the dialog so
    /// in-dialog requests like REFER can be routed).
    fn ack(&self, call_id: &str, from_tag: &str, to_tag: &str, branch: &str) -> String {
        format!(
            "ACK sip:bot@{server} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:{media_port};branch={branch};rport\r\n\
             From: <sip:caller@127.0.0.1>;tag={from_tag}\r\n\
             To: <sip:bot@{server}>;tag={to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 ACK\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\
             \r\n",
            server = self.server,
            media_port = self.socket.local_addr().unwrap().port(),
        )
    }

    /// Build an in-dialog REFER (RFC 3515) transferring the call to `refer_to`.
    fn refer(
        &self,
        call_id: &str,
        from_tag: &str,
        to_tag: &str,
        branch: &str,
        refer_to: &str,
    ) -> String {
        format!(
            "REFER sip:bot@{server} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:{media_port};branch={branch};rport\r\n\
             From: <sip:caller@127.0.0.1>;tag={from_tag}\r\n\
             To: <sip:bot@{server}>;tag={to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 2 REFER\r\n\
             Max-Forwards: 70\r\n\
             Refer-To: <{refer_to}>\r\n\
             Referred-By: <sip:caller@127.0.0.1>\r\n\
             Content-Length: 0\r\n\
             \r\n",
            server = self.server,
            media_port = self.socket.local_addr().unwrap().port(),
        )
    }

    /// Receive messages until one contains `needle`, replying 200 OK to any
    /// in-dialog request (NOTIFY etc.) on the way. Returns (matched, matched
    /// message, every full message seen) — assertions on arrival order or
    /// already-consumed messages can use the message list.
    async fn wait_and_reply(
        &self,
        needle: &str,
        timeout: Duration,
    ) -> (bool, Option<String>, Vec<String>) {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut msgs = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return (false, None, msgs);
            }
            let (n, src) =
                match tokio::time::timeout(remaining, self.socket.recv_from(&mut buf)).await {
                    Ok(Ok(x)) => x,
                    _ => return (false, None, msgs),
                };
            let msg = String::from_utf8_lossy(&buf[..n]).to_string();
            let first_line = msg.lines().next().unwrap_or("").to_string();
            info!(%first_line, "UAC received message");
            if msg.contains(needle) {
                msgs.push(msg.clone());
                return (true, Some(msg), msgs);
            }
            msgs.push(msg.clone());
            // Auto-answer in-dialog requests so transactions complete.
            if first_line.starts_with("NOTIFY")
                || first_line.starts_with("UPDATE")
                || first_line.starts_with("INFO")
            {
                let mut via = String::new();
                let mut from = String::new();
                let mut to = String::new();
                let mut call_id = String::new();
                let mut cseq = String::new();
                for line in msg.lines() {
                    for (prefix, slot) in [
                        ("Via:", &mut via),
                        ("From:", &mut from),
                        ("To:", &mut to),
                        ("Call-ID:", &mut call_id),
                        ("CSeq:", &mut cseq),
                    ] {
                        if line.starts_with(prefix) && slot.is_empty() {
                            *slot = line.to_string();
                        }
                    }
                }
                let ok = format!(
                    "SIP/2.0 200 OK\r\n\
                     {via}\r\n\
                     {from}\r\n\
                     {to}\r\n\
                     {call_id}\r\n\
                     {cseq}\r\n\
                     Content-Length: 0\r\n\
                     \r\n"
                );
                self.socket.send_to(ok.as_bytes(), src).await.ok();
            }
        }
    }
}

const PCMU_OFFER: &str = "v=0\r\n\
o=- 123456 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 40000 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=sendrecv\r\n";

/// One PCMU RTP packet (12B header + payload).
fn rtp_packet(seq: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(12 + payload.len());
    pkt.extend_from_slice(&[0x80, 0x00]); // V=2, PT=0 (PCMU)
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&timestamp.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(payload);
    pkt
}

fn is_rtp(msg: &[u8]) -> bool {
    msg.len() >= 12 && (msg[0] & 0xC0) == 0x80
}

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSender = futures::stream::SplitSink<WsStream, Message>;
type WsReceiver = futures::stream::SplitStream<WsStream>;

/// Attach a bot websocket to the ringing call and send `accept`.
async fn attach_and_accept(node: &mut TestNode, call_id: &str) -> (WsSender, WsReceiver) {
    let payload = tokio::time::timeout(Duration::from_secs(5), node.webhook_rx.recv())
        .await
        .expect("webhook not called")
        .expect("webhook channel closed");
    // `sipCallId` carries the raw SIP Call-ID for correlation; `dialogId` is
    // the short public session id (s.<hex>) used to attach the websocket.
    assert_eq!(
        payload.sip_call_id, call_id,
        "unexpected sip call id {}",
        payload.sip_call_id
    );
    let dialog_id = payload.dialog_id;
    assert!(
        dialog_id.starts_with("s."),
        "unexpected session id {dialog_id}"
    );
    info!(%dialog_id, "got session id from webhook");

    let (ws, _) = connect_async(format!(
        "ws://127.0.0.1:{}/call?id={dialog_id}",
        node.http_port
    ))
    .await
    .expect("failed to attach websocket to the ringing call");
    let (mut sink, mut stream) = ws.split();

    // Send accept once the call is attached (first event confirms attach).
    let _ = stream.next().await; // trackStart or similar early event
    sink.send(Message::text(
        r#"{"command":"accept","option":{}}"#.to_string(),
    ))
    .await
    .expect("failed to send accept");

    (sink, stream)
}

/// A WebSocket-type call attached to a ringing inbound SIP dialog must answer
/// the dialog (200 OK + SDP) when the bot client sends `accept`.
#[tokio::test]
async fn ws_accept_answers_pending_sip_dialog() {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let mut node = spawn_node(35070, vec![]).await;
    let uac = SipUac::new(format!("127.0.0.1:{}", node.sip_port).parse().unwrap(), 0).await;
    let call_id = "ws-accept-regression@127.0.0.1";
    uac.socket
        .send_to(
            uac.invite(call_id, "fromtag1", "z9hG4bKwsaccept1", PCMU_OFFER)
                .as_bytes(),
            uac.server,
        )
        .await
        .unwrap();

    let (_bot_sink, _bot_stream) = attach_and_accept(&mut node, call_id).await;

    // The SIP dialog must now be answered with 200 OK (+ SDP).
    let (answered, ok_msg, seen) = uac
        .wait_for_status("SIP/2.0 200", Duration::from_secs(8))
        .await;
    assert!(
        answered,
        "SIP dialog was never answered with 200 OK after websocket accept; \
         responses seen: {seen:?}"
    );
    let ok_msg = ok_msg.unwrap();
    assert!(
        ok_msg.contains("m=audio"),
        "200 OK must carry an SDP answer, got: {ok_msg}"
    );
}

/// After the websocket accept, audio must flow in BOTH directions between
/// the SIP caller and the websocket bot. This is the production failure
/// mode where the refer-connected agent heard only dead silence.
#[tokio::test]
async fn ws_accept_bridges_bidirectional_media() {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let mut node = spawn_node(35071, vec![]).await;
    let media_port = 41000;
    let uac = SipUac::new(
        format!("127.0.0.1:{}", node.sip_port).parse().unwrap(),
        media_port,
    )
    .await;
    let call_id = "ws-media-regression@127.0.0.1";
    uac.socket
        .send_to(
            uac.invite(call_id, "fromtag2", "z9hG4bKwsmedia1", PCMU_OFFER)
                .as_bytes(),
            uac.server,
        )
        .await
        .unwrap();

    let (mut bot_sink, mut bot_stream) = attach_and_accept(&mut node, call_id).await;

    let (answered, ok_msg, seen) = uac
        .wait_for_status("SIP/2.0 200", Duration::from_secs(8))
        .await;
    assert!(
        answered,
        "SIP dialog was never answered with 200 OK after websocket accept; \
         responses seen: {seen:?}"
    );
    let (host, port) = SipUac::answer_media_endpoint(&ok_msg.unwrap());
    info!(%host, port, "answer media endpoint");
    let media_addr: std::net::SocketAddr = format!("{host}:{port}").parse().unwrap();

    // ── Uplink: customer RTP must reach the bot as websocket binary audio.
    let payload = vec![0xFFu8; 160]; // 20ms PCMU
    for seq in 0..25u16 {
        let pkt = rtp_packet(seq, seq as u32 * 160, 0x1234_5678, &payload);
        uac.socket.send_to(&pkt, media_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut uplink_bytes = 0usize;
    let uplink_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = uplink_deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "bot never received uplink audio over websocket"
        );
        match tokio::time::timeout(remaining, bot_stream.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) if !data.is_empty() => {
                uplink_bytes += data.len();
                if uplink_bytes >= 160 {
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue, // text events
            Ok(Some(Err(e))) => panic!("websocket error while waiting for uplink audio: {e}"),
            Ok(None) => panic!("websocket closed while waiting for uplink audio"),
            Err(_) => continue,
        }
    }
    info!(uplink_bytes, "uplink audio reached the websocket bot");

    // ── Downlink: bot audio must reach the customer as RTP. This is the
    // direction that carried ZERO packets in production (agent heard silence).
    let pcm_frame = vec![0x00u8; 640]; // 20ms of 16kHz mono PCM
    for _ in 0..50 {
        bot_sink
            .send(Message::Binary(pcm_frame.clone().into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut downlink_packets = 0usize;
    let downlink_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 4096];
    loop {
        let remaining = downlink_deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "customer never received downlink RTP (production one-way-audio symptom)"
        );
        match tokio::time::timeout(remaining, uac.socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                if is_rtp(&buf[..n]) {
                    downlink_packets += 1;
                    if downlink_packets >= 3 {
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    info!(downlink_packets, "downlink RTP reached the SIP caller");
}

/// A minimal SIP UAS that answers a refer INVITE (the "agent" side).
struct AgentUas {
    sip_socket: UdpSocket,
    media_socket: UdpSocket,
    media_port: u16,
}

impl AgentUas {
    async fn new(media_port: u16) -> Self {
        let sip_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let media_socket = UdpSocket::bind(("127.0.0.1", media_port)).await.unwrap();
        Self {
            sip_socket,
            media_socket,
            media_port,
        }
    }

    /// The SIP URI the refer must target (includes the agent's signaling port).
    fn uri(&self) -> String {
        format!(
            "sip:agent@127.0.0.1:{}",
            self.sip_socket.local_addr().unwrap().port()
        )
    }

    /// Wait for the refer INVITE and answer it with 200 OK. Returns the raw
    /// INVITE message and the peer address (for the follow-up BYE).
    async fn answer_refer_invite(&self, timeout: Duration) -> (String, std::net::SocketAddr) {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "agent never received the refer INVITE"
            );
            let (n, src) = tokio::time::timeout(remaining, self.sip_socket.recv_from(&mut buf))
                .await
                .expect("recv failed")
                .expect("recv error");
            let msg = String::from_utf8_lossy(&buf[..n]).to_string();
            if !msg.starts_with("INVITE") {
                info!(status = %msg.lines().next().unwrap_or(""), "agent ignoring non-INVITE");
                continue;
            }
            info!("agent received refer INVITE");

            // Build the 200 OK from the request's Via/From/Call-ID/CSeq.
            let mut via = String::new();
            let mut from = String::new();
            let mut call_id = String::new();
            let mut cseq = String::new();
            for line in msg.lines() {
                if line.starts_with("Via:") {
                    via = line.to_string();
                } else if line.starts_with("From:") {
                    from = line.to_string();
                } else if line.starts_with("Call-ID:") {
                    call_id = line.to_string();
                } else if line.starts_with("CSeq:") {
                    cseq = line.to_string();
                }
            }
            let sdp = format!(
                "v=0\r\n\
                 o=- 987654 1 IN IP4 127.0.0.1\r\n\
                 s=-\r\n\
                 c=IN IP4 127.0.0.1\r\n\
                 t=0 0\r\n\
                 m=audio {port} RTP/AVP 0\r\n\
                 a=rtpmap:0 PCMU/8000\r\n\
                 a=sendrecv\r\n",
                port = self.media_port
            );
            let ok = format!(
                "SIP/2.0 200 OK\r\n\
                 {via}\r\n\
                 {from}\r\n\
                 To: <sip:agent@127.0.0.1>;tag=agenttag1\r\n\
                 {call_id}\r\n\
                 {cseq}\r\n\
                 Contact: <sip:agent@127.0.0.1:{media_port}>\r\n\
                 Content-Type: application/sdp\r\n\
                 Content-Length: {len}\r\n\
                 \r\n\
                 {sdp}",
                media_port = self.media_port,
                len = sdp.len()
            );
            self.sip_socket.send_to(ok.as_bytes(), src).await.unwrap();
            info!("agent answered the refer INVITE");
            return (msg, src);
        }
    }

    /// Tear the refer leg down (agent hangs up first). The PBX must then
    /// hang up the parent (customer) dialog via the auto-hangup path.
    async fn send_bye(&self, invite: &str, dst: std::net::SocketAddr) {
        let mut call_id = String::new();
        let mut remote_from = String::new();
        let mut contact = String::new();
        for line in invite.lines() {
            if line.starts_with("Call-ID:") {
                call_id = line.to_string();
            } else if line.starts_with("From:") && remote_from.is_empty() {
                // The INVITE's From (the PBX side, with its tag) becomes the
                // BYE's To header.
                remote_from = format!(
                    "To:{}",
                    line.strip_prefix("From:").unwrap_or("").to_string()
                );
            } else if line.starts_with("Contact:") && contact.is_empty() {
                contact = line
                    .strip_prefix("Contact:")
                    .unwrap_or("")
                    .trim()
                    .trim_matches(['<', '>'])
                    .to_string();
            }
        }
        let port = self.sip_socket.local_addr().unwrap().port();
        let bye = format!(
            "BYE {contact} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:{port};branch=z9hG4bKagentbye1;rport\r\n\
             From: <sip:agent@127.0.0.1>;tag=agenttag1\r\n\
             {remote_from}\r\n\
             {call_id}\r\n\
             CSeq: 2 BYE\r\n\
             Content-Length: 0\r\n\
             \r\n"
        );
        self.sip_socket.send_to(bye.as_bytes(), dst).await.unwrap();
        info!("agent sent BYE on the refer leg");
    }

    /// Receive RTP packets (customer audio forwarded through the refer leg);
    /// returns the PBX media source address once enough packets arrived.
    async fn wait_rtp(&self, timeout: Duration) -> Option<std::net::SocketAddr> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        let mut packets = 0;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, self.media_socket.recv_from(&mut buf)).await {
                Ok(Ok((n, src))) => {
                    if is_rtp(&buf[..n]) {
                        packets += 1;
                        info!(packets, %src, "agent received RTP");
                        if packets >= 3 {
                            return Some(src);
                        }
                    }
                }
                _ => continue,
            }
        }
    }
}

/// Full production scenario: carrier INVITE -> websocket bot accept ->
/// refer to an agent -> customer and agent exchange audio bidirectionally.
/// This is the zapbx + VOS3000 + TianRun IVR transfer flow that used to
/// leave the customer leg unanswered and the agent in dead silence.
#[tokio::test]
async fn ws_refer_connects_customer_and_agent_media() {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let mut node = spawn_node(35072, vec!["pcmu".to_string(), "pcma".to_string()]).await;
    let customer_media_port = 42000;
    let customer = SipUac::new(
        format!("127.0.0.1:{}", node.sip_port).parse().unwrap(),
        customer_media_port,
    )
    .await;
    let call_id = "ws-refer-regression@127.0.0.1";
    customer
        .socket
        .send_to(
            customer
                .invite(call_id, "fromtag3", "z9hG4bKwsrefer1", PCMU_OFFER)
                .as_bytes(),
            customer.server,
        )
        .await
        .unwrap();

    let (mut bot_sink, _bot_stream) = attach_and_accept(&mut node, call_id).await;

    // Customer leg must be established before the transfer.
    let (answered, ok_msg, seen) = customer
        .wait_for_status("SIP/2.0 200", Duration::from_secs(8))
        .await;
    assert!(
        answered,
        "customer dialog never answered; responses seen: {seen:?}"
    );
    let (chost, cport) = SipUac::answer_media_endpoint(&ok_msg.unwrap());
    let customer_media_addr: std::net::SocketAddr = format!("{chost}:{cport}").parse().unwrap();

    // The bot transfers the call to the agent. The callee URI carries the
    // agent's signaling port; the node is configured with pcmu/pcma codecs
    // (as production zapbx does) so the refer INVITE offers PCMU.
    let agent = AgentUas::new(42100).await;
    bot_sink
        .send(Message::text(
            format!(
                r#"{{"command":"refer","caller":"sip:caller@127.0.0.1","callee":"{}","options":{{"autoHangup":true,"timeout":10}}}}"#,
                agent.uri()
            ),
        ))
        .await
        .expect("failed to send refer");

    agent.answer_refer_invite(Duration::from_secs(8)).await;

    // ── Customer -> agent audio must flow through the refer bridge.
    let payload = vec![0x55u8; 160];
    for seq in 0..40u16 {
        let pkt = rtp_packet(seq, seq as u32 * 160, 0xABCD_0001, &payload);
        customer
            .socket
            .send_to(&pkt, customer_media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pbx_refer_addr = agent.wait_rtp(Duration::from_secs(5)).await.expect(
        "agent never received customer audio after refer (production one-way-audio symptom)",
    );

    // ── Agent -> customer audio must flow as well. The PBX's refer leg
    // latches onto the agent's media endpoint (127.0.0.1:{agent.media_port}
    // from the 200 OK SDP), so audio sent from that socket is relayed to the
    // customer. Reply to the PBX media source discovered above.
    for seq in 0..40u16 {
        let pkt = rtp_packet(seq, seq as u32 * 160, 0xABCD_0003, &payload);
        agent
            .media_socket
            .send_to(&pkt, pbx_refer_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut downlink = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut buf2 = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "customer never received agent audio after refer"
        );
        match tokio::time::timeout(remaining, customer.socket.recv_from(&mut buf2)).await {
            Ok(Ok((n, _))) => {
                if is_rtp(&buf2[..n]) {
                    downlink += 1;
                    if downlink >= 3 {
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    info!(
        downlink,
        "customer received agent audio through the refer bridge"
    );
}

/// A UAS that rejects every INVITE with 486 Busy Here — a transfer target
/// that fails fast (no handshake timeout needed).
struct BusyUas {
    socket: UdpSocket,
}

impl BusyUas {
    async fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        Self { socket }
    }

    fn uri(&self) -> String {
        format!(
            "sip:busy@127.0.0.1:{}",
            self.socket.local_addr().unwrap().port()
        )
    }

    /// Wait for an INVITE and reject it with 486.
    async fn reject_next_invite(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "busy target never received an INVITE");
            let (n, src) = tokio::time::timeout(remaining, self.socket.recv_from(&mut buf))
                .await
                .expect("recv failed")
                .expect("recv error");
            let msg = String::from_utf8_lossy(&buf[..n]).to_string();
            if !msg.starts_with("INVITE") {
                continue;
            }
            let mut via = String::new();
            let mut from = String::new();
            let mut call_id = String::new();
            let mut cseq = String::new();
            for line in msg.lines() {
                if line.starts_with("Via:") {
                    via = line.to_string();
                } else if line.starts_with("From:") {
                    from = line.to_string();
                } else if line.starts_with("Call-ID:") {
                    call_id = line.to_string();
                } else if line.starts_with("CSeq:") {
                    cseq = line.to_string();
                }
            }
            let reject = format!(
                "SIP/2.0 486 Busy Here\r\n\
                 {via}\r\n\
                 {from}\r\n\
                 To: <sip:busy@127.0.0.1>;tag=busytag1\r\n\
                 {call_id}\r\n\
                 {cseq}\r\n\
                 Content-Length: 0\r\n\
                 \r\n"
            );
            self.socket.send_to(reject.as_bytes(), src).await.unwrap();
            info!("busy target rejected the refer INVITE with 486");
            return;
        }
    }
}

/// Read websocket events until one with the given `event` name arrives.
async fn wait_ws_event(
    stream: &mut WsReceiver,
    name: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if v.get("event").and_then(|e| e.as_str()) == Some(name) {
                        return Some(v);
                    }
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("websocket error while waiting for {name}: {e}"),
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

/// Incoming in-dialog REFER (RFC 3515): the websocket bot's call is
/// transferred to the agent. The referrer must see 202, an implicit
/// subscription with NOTIFY 100 Trying, a final NOTIFY 200 once the refer
/// leg answers, and a BYE on the parent dialog once the agent hangs up.
#[tokio::test]
async fn incoming_refer_transfers_call_and_notifies_referrer() {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let mut node = spawn_node(35073, vec!["pcmu".to_string()]).await;
    let customer = SipUac::new(
        format!("127.0.0.1:{}", node.sip_port).parse().unwrap(),
        42010,
    )
    .await;
    let call_id = "incoming-refer-success@127.0.0.1";
    customer
        .socket
        .send_to(
            customer
                .invite(call_id, "fromtag-r1", "z9hG4bKinref1", PCMU_OFFER)
                .as_bytes(),
            customer.server,
        )
        .await
        .unwrap();

    let (mut bot_sink, mut bot_stream) = attach_and_accept(&mut node, call_id).await;

    let (answered, ok_msg, seen) = customer
        .wait_for_status("SIP/2.0 200", Duration::from_secs(8))
        .await;
    assert!(answered, "call not answered; seen: {seen:?}");
    let ok_msg = ok_msg.unwrap();
    let to_tag = SipUac::header_tag(&ok_msg, "To:").expect("200 OK missing To tag");
    let _ = SipUac::answer_media_endpoint(&ok_msg);

    // Confirm the dialog (ACK) so in-dialog requests can be routed.
    customer
        .socket
        .send_to(
            customer
                .ack(call_id, "fromtag-r1", &to_tag, "z9hG4bKinrefack1")
                .as_bytes(),
            customer.server,
        )
        .await
        .unwrap();

    // ── Customer sends an in-dialog REFER transferring to the agent.
    let agent = AgentUas::new(42101).await;
    customer
        .socket
        .send_to(
            customer
                .refer(
                    call_id,
                    "fromtag-r1",
                    &to_tag,
                    "z9hG4bKinrefbye1",
                    &agent.uri(),
                )
                .as_bytes(),
            customer.server,
        )
        .await
        .unwrap();

    // RFC 3515 sequence: 202 first, then the implicit subscription opens
    // with a 100 Trying / active NOTIFY. The two are read from the same
    // socket, so the 202 is asserted from the message log.
    let (trying, _, msgs) = customer
        .wait_and_reply("Subscription-State: active", Duration::from_secs(8))
        .await;
    assert!(
        trying,
        "no active NOTIFY (100 Trying) for the refer subscription; messages: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("SIP/2.0 202")),
        "REFER was never answered with 202: {msgs:?}"
    );

    // The bot sees the transferRequest event...
    let tr = wait_ws_event(&mut bot_stream, "transferRequest", Duration::from_secs(8))
        .await
        .expect("websocket bot never received transferRequest");
    assert!(
        tr.get("referTo")
            .and_then(|v| v.as_str())
            .map(|v| v.contains(&agent.uri()))
            .unwrap_or(false),
        "transferRequest carries unexpected referTo: {tr}"
    );

    // ...and the refer leg reaches the agent, which answers.
    let (invite_msg, agent_peer) = agent.answer_refer_invite(Duration::from_secs(8)).await;

    // Final NOTIFY: transfer succeeded (200, terminated).
    let (final_ok, final_msg, msgs) = customer
        .wait_and_reply("Subscription-State: terminated", Duration::from_secs(8))
        .await;
    assert!(
        final_ok,
        "no terminated NOTIFY for the refer subscription; messages: {msgs:?}"
    );
    let final_msg = final_msg.unwrap();
    assert!(
        final_msg.contains("SIP/2.0 200"),
        "terminated NOTIFY should report success, got: {final_msg}"
    );
    assert!(
        final_msg.contains("Event: refer"),
        "NOTIFY must use the refer event package, got: {final_msg}"
    );

    // WS answer event marks the refer leg.
    let answer = wait_ws_event(&mut bot_stream, "answer", Duration::from_secs(8))
        .await
        .expect("websocket bot never received refer answer event");
    assert_eq!(
        answer.get("refer").and_then(|v| v.as_bool()),
        Some(true),
        "answer event should carry refer=true: {answer}"
    );

    // ── Agent hangs up the refer leg; the parent (customer) dialog must be
    // hung up by the auto-hangup path.
    agent.send_bye(&invite_msg, agent_peer).await;
    let (bye, _, seen) = customer
        .wait_and_reply("BYE ", Duration::from_secs(8))
        .await;
    assert!(
        bye,
        "customer dialog was never hung up after the refer leg ended; seen: {seen:?}"
    );

    let hangup = wait_ws_event(&mut bot_stream, "hangup", Duration::from_secs(8)).await;
    assert!(hangup.is_some(), "websocket bot never received hangup");
    let _ = bot_sink
        .send(Message::text(r#"{"command":"hangup"}"#.to_string()))
        .await;
}

/// Incoming REFER to a target that rejects with 486: the referrer gets a
/// failure NOTIFY and the parent dialog must stay alive.
#[tokio::test]
async fn incoming_refer_failure_notifies_and_keeps_call_alive() {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let mut node = spawn_node(35074, vec!["pcmu".to_string()]).await;
    let customer = SipUac::new(
        format!("127.0.0.1:{}", node.sip_port).parse().unwrap(),
        42011,
    )
    .await;
    let call_id = "incoming-refer-failure@127.0.0.1";
    customer
        .socket
        .send_to(
            customer
                .invite(call_id, "fromtag-r2", "z9hG4bKinref2", PCMU_OFFER)
                .as_bytes(),
            customer.server,
        )
        .await
        .unwrap();

    let (_bot_sink, mut bot_stream) = attach_and_accept(&mut node, call_id).await;

    let (answered, ok_msg, seen) = customer
        .wait_for_status("SIP/2.0 200", Duration::from_secs(8))
        .await;
    assert!(answered, "call not answered; seen: {seen:?}");
    let to_tag = SipUac::header_tag(&ok_msg.unwrap(), "To:").expect("200 OK missing To tag");

    // Confirm the dialog (ACK) so in-dialog requests can be routed.
    customer
        .socket
        .send_to(
            customer
                .ack(call_id, "fromtag-r2", &to_tag, "z9hG4bKinrefack2")
                .as_bytes(),
            customer.server,
        )
        .await
        .unwrap();

    // Transfer to a target that rejects with 486.
    let busy = BusyUas::new().await;
    customer
        .socket
        .send_to(
            customer
                .refer(
                    call_id,
                    "fromtag-r2",
                    &to_tag,
                    "z9hG4bKinrefbye2",
                    &busy.uri(),
                )
                .as_bytes(),
            customer.server,
        )
        .await
        .unwrap();

    // 202 + active NOTIFY (order on the wire: 202 before NOTIFY; the 202 is
    // asserted from the message log).
    let (trying, _, msgs) = customer
        .wait_and_reply("Subscription-State: active", Duration::from_secs(8))
        .await;
    assert!(
        trying,
        "no active NOTIFY (100 Trying) for the refer subscription; messages: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("SIP/2.0 202")),
        "REFER was never answered with 202: {msgs:?}"
    );

    // The refer leg INVITE fails fast with 486.
    busy.reject_next_invite(Duration::from_secs(8)).await;

    // Final NOTIFY reports the failure.
    let (terminated, final_msg, msgs) = customer
        .wait_and_reply("Subscription-State: terminated", Duration::from_secs(10))
        .await;
    assert!(
        terminated,
        "no terminated NOTIFY for the failed refer; messages: {msgs:?}"
    );
    let final_msg = final_msg.unwrap();
    assert!(
        final_msg.contains("486"),
        "terminated NOTIFY should report the 486 failure, got: {final_msg}"
    );

    // transferRequest was still emitted (WS clients can take over manually).
    let tr = wait_ws_event(&mut bot_stream, "transferRequest", Duration::from_secs(8))
        .await
        .expect("websocket bot never received transferRequest");
    assert!(
        tr.get("referTo").is_some(),
        "transferRequest missing referTo"
    );

    // The parent dialog stays alive: no BYE within the window.
    let (bye, _, msgs) = customer
        .wait_and_reply("BYE ", Duration::from_secs(3))
        .await;
    assert!(
        !bye,
        "customer dialog must NOT be hung up after a failed transfer; messages: {msgs:?}"
    );
}

/// Regression test: a far-end CANCEL arriving while the INVITE is still
/// ringing — after the websocket client has attached but before it sent any
/// command (Ringing/Accept) — must terminate the attached call. Before the
/// fix the attached ActiveCall only started watching the dialog state on its
/// first command, so the CANCEL left a zombie entry in `active_calls`
/// (`/call/list` showed a call with no option/ring/answer times) and the
/// websocket kept pinging forever.
#[tokio::test]
async fn ws_cancel_before_first_command_tears_down_attached_call() {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_test_writer()
        .try_init()
        .ok();

    let mut node = spawn_node(35073, vec![]).await;
    let uac = SipUac::new(
        format!("127.0.0.1:{}", node.sip_port).parse().unwrap(),
        0,
    )
    .await;
    let call_id = "ws-cancel-regression@127.0.0.1";
    let from_tag = "fromtag4";
    let branch = "z9hG4bKwscancel1";
    uac.socket
        .send_to(
            uac.invite(call_id, from_tag, branch, PCMU_OFFER).as_bytes(),
            uac.server,
        )
        .await
        .unwrap();

    // Attach the websocket to the ringing call, but send NO command — this is
    // the window where the dialog-state watcher is not yet in place.
    let payload = tokio::time::timeout(Duration::from_secs(5), node.webhook_rx.recv())
        .await
        .expect("webhook not called")
        .expect("webhook channel closed");
    let dialog_id = payload.dialog_id;
    let (ws, _) = connect_async(format!(
        "ws://127.0.0.1:{}/call?id={dialog_id}",
        node.http_port
    ))
    .await
    .expect("failed to attach websocket to the ringing call");
    let (_sink, mut stream) = ws.split();
    let _ = stream.next().await; // first event confirms the call is attached

    // The caller gives up before the bot ever sends a command.
    uac.socket
        .send_to(uac.cancel(call_id, from_tag, branch).as_bytes(), uac.server)
        .await
        .unwrap();

    // SIP side: the INVITE must end with 487 Request Terminated.
    let (terminated, _msg, seen) = uac
        .wait_for_status("SIP/2.0 487", Duration::from_secs(8))
        .await;
    assert!(
        terminated,
        "INVITE was never terminated with 487 after CANCEL; responses seen: {seen:?}"
    );

    // WS side: the attached call must report the hangup and close. Without the
    // fix the stream just kept emitting pings until the test timed out.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut hangup_seen = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "websocket was not closed after CANCEL (zombie call)"
        );
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                info!(%text, "bot received event");
                if text.contains("\"event\":\"hangup\"") {
                    assert!(
                        text.contains("Canceled"),
                        "hangup event must carry the Canceled reason: {text}"
                    );
                    hangup_seen = true;
                }
            }
            Ok(Some(Ok(_))) => continue, // pings/binary
            // The server tears the connection down right after its Close
            // frame, so the final read may surface as an IO error instead of
            // a clean stream end.
            Ok(Some(Err(_))) => break,
            Ok(None) => break, // closed by the server after teardown
            Err(_) => continue,
        }
    }
    assert!(
        hangup_seen,
        "websocket closed without a hangup event for the cancelled call"
    );

    // The call must be gone from the registry.
    let list = reqwest::get(format!("http://127.0.0.1:{}/list", node.http_port))
        .await
        .expect("failed to query /list")
        .json::<serde_json::Value>()
        .await
        .expect("failed to parse /list response");
    let active = list["active_calls"].as_array().cloned().unwrap_or_default();
    assert!(
        active.is_empty(),
        "cancelled call lingered in /list: {active:?}"
    );
}
