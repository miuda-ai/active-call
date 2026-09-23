//! End-to-end test for the INVITE-mode auto-refer transfer (production
//! scenario seen with Aliyun CC + zapbx):
//!
//! 1. A customer (raw SIP UAC) calls in; a WebSocket bot accepts.
//! 2. The bot sends `refer` — the PBX dials a carrier bridge leg (Bot B).
//! 3. Bot B answers, then sends an **in-dialog REFER** on the bridge leg
//!    with `Refer-To: sip:952299@<agent>` — exactly where INVITE-mode
//!    carriers deliver their transfer instruction.
//! 4. The PBX must follow it: 202 + NOTIFY (Bot B rejects NOTIFY with 405,
//!    like Aliyun's SBC), then a **new transfer INVITE to the agent (Bot C)**.
//! 5. When the agent answers, the PBX retires the bridge leg (BYE) while the
//!    customer stays connected; the session only ends when the agent hangs up.

use active_call::{
    app::{AppState, AppStateBuilder},
    config::{Config, InviteHandlerConfig},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::Message,
    MaybeTlsStream, WebSocketStream,
};
use tracing::{info, Level};

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

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSender = futures::stream::SplitSink<WsStream, Message>;
type WsReceiver = futures::stream::SplitStream<WsStream>;

async fn spawn_node(sip_port: u16) -> TestNode {
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
        rtp_start_port: Some(32000 + port_key * 20),
        rtp_end_port: Some(32020 + port_key * 20),
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

    TestNode {
        http_port,
        sip_port,
        webhook_rx,
    }
}

async fn attach_and_accept(node: &mut TestNode) -> (WsSender, mpsc::UnboundedReceiver<String>) {
    let payload = tokio::time::timeout(Duration::from_secs(8), node.webhook_rx.recv())
        .await
        .expect("webhook not called")
        .expect("webhook channel closed");
    let dialog_id = payload.dialog_id;
    info!(%dialog_id, "got session id from webhook");

    let (ws, _) = connect_async(format!(
        "ws://127.0.0.1:{}/call?id={dialog_id}",
        node.http_port
    ))
    .await
    .expect("failed to attach websocket to the ringing call");
    let (mut sink, mut stream) = ws.split();

    let _ = stream.next().await; // early attach event
    sink.send(Message::text(r#"{"command":"accept","option":{}}"#.to_string()))
        .await
        .expect("failed to send accept");

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Text(t) = msg {
                let _ = event_tx.send(t.to_string());
            }
        }
    });
    (sink, event_rx)
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

const BOT_SDP: &str = "v=0\r\n\
o=- 777 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio {port} RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendrecv\r\n";

fn is_rtp(msg: &[u8]) -> bool {
    msg.len() >= 12 && (msg[0] & 0xC0) == 0x80
}

/// Raw-UDP SIP helpers shared by the fake carrier (Bot B) and agent (Bot C).
struct SipSock {
    socket: UdpSocket,
    /// Tag we generated in our 200 OK (needed to build in-dialog requests).
    to_tag: Option<String>,
}

impl SipSock {
    async fn bind() -> Self {
        Self {
            socket: UdpSocket::bind(("127.0.0.1", 0)).await.unwrap(),
            to_tag: None,
        }
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    async fn send_raw(&self, msg: &str, dst: std::net::SocketAddr) {
        self.socket.send_to(msg.as_bytes(), dst).await.unwrap();
    }

    /// Receive the next SIP message (skipping RTP), with timeout.
    async fn recv_sip(&self, timeout: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, self.socket.recv_from(&mut buf)).await {
                Ok(Ok((n, src))) => {
                    if is_rtp(&buf[..n]) {
                        continue;
                    }
                    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
                    info!(%src, first = %msg.lines().next().unwrap_or(""), "bot received");
                    return Some(msg);
                }
                _ => return None,
            }
        }
    }

    fn hdr<'a>(&self, msg: &'a str, name: &str) -> Option<String> {
        msg.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(name) {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
    }

    fn dst(&self, msg: &str) -> std::net::SocketAddr {
        // Respond to the address in the top Via header (all peers are local).
        let via = self.hdr(msg, "Via").unwrap_or_default();
        let received = msg
            .lines()
            .find_map(|l| {
                let lower = l.to_ascii_lowercase();
                if lower.starts_with("via:") && lower.contains("received=") {
                    lower.split("received=").nth(1).and_then(|r| {
                        r.split(|c: char| !c.is_ascii_digit() && c != '.')
                            .next()
                            .map(|s| s.to_string())
                    })
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = via
            .split("rport=")
            .nth(1)
            .and_then(|p| p.split(';').next())
            .and_then(|p| p.parse().ok())
            .or_else(|| {
                via.rsplit(':')
                    .next()
                    .and_then(|p| p.split(';').next())
                    .and_then(|p| p.parse().ok())
            })
            .unwrap_or(5060);
        format!("{received}:{port}").parse().unwrap()
    }

    fn reply(&self, req: &str, code_line: &str, extra: Option<&str>) {
        let via = self.hdr(req, "Via").unwrap_or_default();
        let from = self.hdr(req, "From").unwrap_or_default();
        let to = self.hdr(req, "To").unwrap_or_default();
        let call_id = self.hdr(req, "Call-ID").unwrap_or_default();
        let cseq = self.hdr(req, "CSeq").unwrap_or_default();
        let body = extra.unwrap_or("");
        let resp = format!(
            "{code_line}\r\n\
             Via: {via}\r\n\
             From: {from}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: {}\r\n\
             \r\n\
             {body}",
            body.len()
        );
        let dst = self.dst(req);
        futures::executor::block_on(self.send_raw(&resp, dst));
    }

    fn answer_ok(&mut self, req: &str) {
        let sdp = BOT_SDP.replace("{port}", &self.port().to_string());
        let via = self.hdr(req, "Via").unwrap_or_default();
        let from = self.hdr(req, "From").unwrap_or_default();
        let to = self.hdr(req, "To").unwrap_or_default();
        let tag = format!("bot{}", rand::random::<u32>());
        let to = format!("{to};tag={tag}");
        self.to_tag = Some(tag);
        let call_id = self.hdr(req, "Call-ID").unwrap_or_default();
        let cseq = self.hdr(req, "CSeq").unwrap_or_default();
        let contact = format!("<sip:bot@127.0.0.1:{}>", self.port());
        let resp = format!(
            "SIP/2.0 200 OK\r\n\
             Via: {via}\r\n\
             From: {from}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq}\r\n\
             Contact: {contact}\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {}\r\n\
             \r\n\
             {sdp}",
            sdp.len()
        );
        let dst = self.dst(req);
        futures::executor::block_on(self.send_raw(&resp, dst));
    }

    /// Send an in-dialog request (REFER/BYE) using the dialog data of `req`.
    fn send_in_dialog(&self, invite_req: &str, method: &str, extra_headers: &[&str], cseq: u32) {
        // Dialog: local (From) = the INVITE's To + our tag from the 200 OK;
        // remote (To) = the INVITE's From (carries their tag already).
        let to = self.hdr(invite_req, "From").unwrap_or_default();
        let from = match self.to_tag.as_deref() {
            Some(tag) => format!("{};tag={tag}", self.hdr(invite_req, "To").unwrap_or_default()),
            None => self.hdr(invite_req, "To").unwrap_or_default(),
        };
        let call_id = self.hdr(invite_req, "Call-ID").unwrap_or_default();
        let contact = format!("<sip:bot@127.0.0.1:{}>", self.port());
        let mut req = format!(
            "{method} sip:bot@127.0.0.1:{} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:{};branch=z9hG4bKbot{}\r\n\
             From: {from}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Contact: {contact}\r\n\
             Max-Forwards: 70\r\n",
            self.port(),
            self.port(),
            cseq
        );
        for h in extra_headers {
            req.push_str(h);
            req.push_str("\r\n");
        }
        req.push_str("Content-Length: 0\r\n\r\n");
        let dst = self.dst(invite_req);
        futures::executor::block_on(self.send_raw(&req, dst));
    }
}

/// Bot B — fake INVITE-mode carrier bridge: answers, then REFERs the call to
/// the agent, rejects NOTIFY with 405 (like Aliyun's SBC), expects BYE.
async fn run_bridge_bot(mut sock: SipSock, agent_port: u16, events: mpsc::UnboundedSender<String>) {
    let invite = loop {
        match sock.recv_sip(Duration::from_secs(20)).await {
            Some(msg) if msg.starts_with("INVITE") => break msg,
            Some(msg) => sock.reply(&msg, "SIP/2.0 488 Not Acceptable Here", None),
            None => {
                let _ = events.send("TIMEOUT: never invited".into());
                return;
            }
        }
    };
    sock.reply(&invite, "SIP/2.0 100 Trying", None);
    sock.answer_ok(&invite);
    let _ = events.send("answered".into());

    // Wait for ACK, skipping any stray packets.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match sock.recv_sip(Duration::from_secs(5)).await {
                Some(msg) if msg.starts_with("ACK") => break,
                Some(_) => continue,
                None => break,
            }
        }
    })
    .await;

    // Send the in-dialog REFER with Refer-To = agent.
    tokio::time::sleep(Duration::from_millis(700)).await;
    sock.send_in_dialog(
        &invite,
        "REFER",
        &[&format!(
            "Refer-To: <sip:952299@127.0.0.1:{agent_port}>"
        )],
        2,
    );
    let _ = events.send("refer_sent".into());

    // Answer loop: 202 for the REFER, 405 for NOTIFYs, 200 for BYE.
    loop {
        match sock.recv_sip(Duration::from_secs(15)).await {
            Some(msg) if msg.starts_with("SIP/2.0 202") => {
                let _ = events.send("refer_accepted".into());
            }
            Some(msg) if msg.starts_with("NOTIFY") => {
                sock.reply(&msg, "SIP/2.0 405 Method Not Allowed", None);
                let _ = events.send("notify_rejected_405".into());
            }
            Some(msg) if msg.starts_with("BYE") => {
                sock.reply(&msg, "SIP/2.0 200 OK", None);
                let _ = events.send("bridge_bye_received".into());
                return;
            }
            Some(msg) if msg.starts_with("SIP/2.0 2") || msg.starts_with("SIP/2.0 1") => {}
            Some(_) => {}
            None => {
                let _ = events.send("TIMEOUT: bridge loop ended".into());
                return;
            }
        }
    }
}

/// Bot C — agent: answers the transfer INVITE, hangs up after a moment.
async fn run_agent_bot(mut sock: SipSock, events: mpsc::UnboundedSender<String>) {
    let invite = loop {
        match sock.recv_sip(Duration::from_secs(25)).await {
            Some(msg) if msg.starts_with("INVITE") => break msg,
            Some(_) => {}
            None => {
                let _ = events.send("TIMEOUT: agent never invited".into());
                return;
            }
        }
    };
    sock.reply(&invite, "SIP/2.0 100 Trying", None);
    sock.answer_ok(&invite);
    let _ = events.send("agent_answered".into());

    // Wait for ACK.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match sock.recv_sip(Duration::from_secs(5)).await {
                Some(msg) if msg.starts_with("ACK") => break,
                Some(_) => continue,
                None => break,
            }
        }
    })
    .await;

    // Talk for a moment, then hang up.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    sock.send_in_dialog(&invite, "BYE", &[], 2);
    loop {
        match sock.recv_sip(Duration::from_secs(8)).await {
            Some(msg) if msg.starts_with("SIP/2.0 200") => {
                let _ = events.send("agent_bye_acknowledged".into());
                return;
            }
            Some(_) => {}
            None => {
                let _ = events.send("TIMEOUT: agent bye unanswered".into());
                return;
            }
        }
    }
}

/// Customer — raw UAC: calls in, then waits for the final BYE.
async fn run_customer(sock: SipSock, pbx: std::net::SocketAddr, events: mpsc::UnboundedSender<String>) {
    let call_id = "auto-refer-customer-1";
    let invite = format!(
        "INVITE sip:100@{pbx} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{port};branch=z9hG4bKcustomer1;rport\r\n\
         From: <sip:1001@127.0.0.1>;tag=customer-tag\r\n\
         To: <sip:100@{pbx}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:1001@127.0.0.1:{port}>\r\n\
         Max-Forwards: 70\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\
         \r\n\
         {PCMU_OFFER}",
        port = sock.port(),
        len = PCMU_OFFER.len(),
    );
    sock.send_raw(&invite, pbx).await;

    let ok = loop {
        match sock.recv_sip(Duration::from_secs(10)).await {
            Some(msg) if msg.starts_with("SIP/2.0 200") => break msg,
            Some(msg) if msg.starts_with("SIP/2.0") => continue,
            Some(_) => continue,
            None => {
                let _ = events.send("TIMEOUT: customer never answered".into());
                return;
            }
        }
    };
    // ACK the 200 OK.
    let via = sock.hdr(&ok, "Via").unwrap_or_default();
    let to = sock.hdr(&ok, "To").unwrap_or_default();
    let ack = format!(
        "ACK sip:100@{pbx} SIP/2.0\r\n\
         Via: {via}\r\n\
         From: <sip:1001@127.0.0.1>;tag=customer-tag\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 ACK\r\n\
         Max-Forwards: 70\r\n\
         Content-Length: 0\r\n\r\n"
    );
    sock.send_raw(&ack, pbx).await;
    let _ = events.send("customer_answered".into());

    // The session must survive the transfer; the final BYE comes only after
    // the agent hangs up.
    loop {
        match sock.recv_sip(Duration::from_secs(25)).await {
            Some(msg) if msg.starts_with("BYE") => {
                sock.reply(&msg, "SIP/2.0 200 OK", None);
                let _ = events.send("customer_got_bye".into());
                return;
            }
            Some(msg) if msg.starts_with("NOTIFY") => {
                sock.reply(&msg, "SIP/2.0 405 Method Not Allowed", None);
            }
            Some(_) => {}
            None => {
                let _ = events.send("TIMEOUT: customer never got BYE".into());
                return;
            }
        }
    }
}

async fn wait_event(
    rx: &mut mpsc::UnboundedReceiver<String>,
    needle: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                if ev.contains(needle) {
                    return Some(ev);
                }
            }
            _ => return None,
        }
    }
}

#[tokio::test]
async fn auto_refer_follows_bridge_leg_refer() {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_test_writer()
        .try_init()
        .ok();

    let mut node = spawn_node(35270).await;
    let pbx = format!("127.0.0.1:{}", node.sip_port).parse().unwrap();

    // Agent (Bot C) first: the bridge bot needs its port for Refer-To.
    let agent_sock = SipSock::bind().await;
    let agent_port = agent_sock.port();
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_agent_bot(agent_sock, agent_tx));

    // Bridge (Bot B).
    let bridge_sock = SipSock::bind().await;
    let bridge_port = bridge_sock.port();
    let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_bridge_bot(bridge_sock, agent_port, bridge_tx));

    // Customer (Bot A) + PBX + webhook.
    let customer_sock = SipSock::bind().await;
    let (customer_tx, mut customer_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_customer(customer_sock, pbx, customer_tx));

    // Attach the WS bot and accept, then trigger the INVITE-mode refer to the bridge.
    let (mut ws_sink, mut ws_events) = attach_and_accept(&mut node).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ws_sink
        .send(Message::text(
            format!(
                r#"{{"command":"refer","caller":"sip:1001@127.0.0.1","callee":"sip:bridge@127.0.0.1:{bridge_port}"}}"#
            ),
        ))
        .await
        .expect("failed to send refer command");

    // 1) Bridge leg answered by Bot B.
    wait_event(&mut bridge_rx, "answered", Duration::from_secs(15))
        .await
        .expect("bridge leg was never answered");

    // 2) Bot B REFERs; PBX must accept it.
    wait_event(&mut bridge_rx, "refer_accepted", Duration::from_secs(10))
        .await
        .expect("REFER was not accepted (202 missing)");

    // 3) THE FIX: the transfer INVITE must reach the agent (Bot C).
    wait_event(&mut agent_rx, "agent_answered", Duration::from_secs(15))
        .await
        .expect("transfer INVITE never reached the agent — auto_refer did not follow the bridge-leg REFER");

    // 4) The WS app observes the refer answer (skip the accept's answer event).
    let mut answer = None;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, ws_events.recv()).await {
                Ok(Some(ev)) => {
                    if ev.contains("\"answer\"") && ev.contains("\"refer\":true") {
                        answer = Some(ev);
                        break;
                    }
                }
                _ => break,
            }
        }
    }
    assert!(
        answer.is_some(),
        "expected an Answer event with refer=true after the transfer INVITE was answered"
    );

    // 5) NOTIFYs are 405'd by the carrier; the PBX must retire the bridge leg.
    wait_event(&mut bridge_rx, "notify_rejected_405", Duration::from_secs(10))
        .await
        .expect("expected at least one NOTIFY towards the referrer");
    wait_event(&mut bridge_rx, "bridge_bye_received", Duration::from_secs(15))
        .await
        .expect("superseded bridge leg was never hung up");

    // 6) Session must still be alive right now: no Hangup event so far.
    let mut premature = None;
    while let Ok(ev) = ws_events.try_recv() {
        if ev.contains("\"hangup\"") {
            premature = Some(ev);
        }
    }
    assert!(
        premature.is_none(),
        "session hung up before the agent: {premature:?}"
    );

    // 7) Agent hangs up -> the whole session tears down -> customer gets BYE.
    wait_event(&mut agent_rx, "agent_bye_acknowledged", Duration::from_secs(10))
        .await
        .expect("agent BYE was not acknowledged");
    wait_event(&mut ws_events, "\"hangup\"", Duration::from_secs(10))
        .await
        .expect("session did not tear down after the agent hung up");
    wait_event(&mut customer_rx, "customer_got_bye", Duration::from_secs(15))
        .await
        .expect("customer never received the final BYE");

    info!("auto-refer bridge test completed successfully");
}
