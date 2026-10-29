//! A WebRTC call whose peer connection is gone for good must hang up on its
//! own. The track used to close on the first `Disconnected` (a state ICE
//! often recovers from) without telling the call: the session stayed up with
//! no media until someone hung up by hand. A connection that recovers within
//! the grace must keep the call.

use active_call::app::AppStateBuilder;
use active_call::callrecord::{CallRecord, CallRecordHangupReason};
use active_call::config::Config;
use active_call::event::SessionEvent;
use active_call::handler::call_router;
use anyhow::Result;
use futures::{SinkExt, StreamExt};
use rustrtc::{
    PeerConnection, PeerConnectionState, RtcConfiguration, media::frame::MediaKind,
    media::sample_track,
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message,
};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A UDP hop between the client and the node, each side sees it as the
/// other. A blackout drops everything both ways, so ICE on both ends sees the
/// path go silent, and come back once it is lifted.
struct Relay {
    /// The node's address as the client sees it.
    to_client: Arc<UdpSocket>,
    /// The client's address as the node sees it.
    to_node: Arc<UdpSocket>,
    client: Arc<Mutex<Option<SocketAddr>>>,
    node: Arc<Mutex<Option<SocketAddr>>>,
    blackout: Arc<AtomicBool>,
}

impl Relay {
    async fn new() -> Result<Self> {
        let relay = Self {
            to_client: Arc::new(UdpSocket::bind("0.0.0.0:0").await?),
            to_node: Arc::new(UdpSocket::bind("0.0.0.0:0").await?),
            client: Default::default(),
            node: Default::default(),
            blackout: Default::default(),
        };
        Self::forward(
            relay.to_client.clone(),
            relay.to_node.clone(),
            relay.client.clone(),
            relay.node.clone(),
            relay.blackout.clone(),
        );
        Self::forward(
            relay.to_node.clone(),
            relay.to_client.clone(),
            relay.node.clone(),
            relay.client.clone(),
            relay.blackout.clone(),
        );
        Ok(relay)
    }

    /// Pass what arrives on `from` on to `dest` through `to`, learning the
    /// sender's address on the way.
    fn forward(
        from: Arc<UdpSocket>,
        to: Arc<UdpSocket>,
        sender: Arc<Mutex<Option<SocketAddr>>>,
        dest: Arc<Mutex<Option<SocketAddr>>>,
        blackout: Arc<AtomicBool>,
    ) {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((len, addr)) = from.recv_from(&mut buf).await {
                *sender.lock().unwrap() = Some(addr);
                if blackout.load(Ordering::SeqCst) {
                    continue;
                }
                let dest = *dest.lock().unwrap();
                if let Some(dest) = dest {
                    to.send_to(&buf[..len], dest).await.ok();
                }
            }
        });
    }

    /// Point the answer's candidate at the relay, remembering the node. The
    /// offer carries no candidates: the node learns the client (the relay)
    /// from its connectivity checks.
    fn answer(&self, sdp: &str) -> Result<String> {
        let (sdp, node) = Self::rewrite_candidates(sdp, self.to_client.local_addr()?.port());
        *self.node.lock().unwrap() = Some(node);
        Ok(sdp)
    }

    /// Keep only the first UDP IPv4 candidate, moved to the relay's port on
    /// the same host; returns the original address.
    fn rewrite_candidates(sdp: &str, relay_port: u16) -> (String, SocketAddr) {
        let mut original = None;
        let lines: Vec<String> = sdp
            .lines()
            .filter_map(|line| {
                if !line.starts_with("a=candidate:") {
                    return Some(line.to_string());
                }
                let mut fields: Vec<&str> = line.split(' ').collect();
                let addr = format!("{}:{}", fields.get(4)?, fields.get(5)?);
                let addr: SocketAddr = addr.parse().ok()?;
                if original.is_some() || !addr.is_ipv4() || !fields[2].eq_ignore_ascii_case("udp") {
                    return None;
                }
                original = Some(addr);
                let port = relay_port.to_string();
                fields[5] = &port;
                Some(fields.join(" "))
            })
            .collect();
        let original = original.expect("no UDP IPv4 candidate in the SDP");
        (lines.join("\r\n") + "\r\n", original)
    }

    fn set_blackout(&self, on: bool) {
        self.blackout.store(on, Ordering::SeqCst);
    }
}

/// Start a node, attach a websocket and connect a rustrtc client to it,
/// through `relay` when given.
async fn connected_call(
    session_id: &str,
    ice_disconnect_grace: &str,
    relay: Option<&Relay>,
) -> Result<(Ws, Arc<PeerConnection>, mpsc::UnboundedReceiver<CallRecord>)> {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port()
    };
    let mut config = Config::default();
    config.http_addr = format!("127.0.0.1:{port}");
    config.udp_port = 0;
    config.ice_disconnect_grace = Some(ice_disconnect_grace.to_string());
    let (record_tx, record_rx) = mpsc::unbounded_channel();
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .with_callrecord_sender(record_tx)
        .build()
        .await?;

    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    let router = call_router().with_state(app_state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.ok() });
    tokio::spawn(async move { app_state.serve().await.ok() });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (mut ws, _) =
        connect_async(format!("ws://127.0.0.1:{port}/call/webrtc?id={session_id}")).await?;

    let pc = Arc::new(PeerConnection::new(RtcConfiguration::default()));
    let (_source, track, _) = sample_track(MediaKind::Audio, 100);
    pc.add_track(
        track,
        rustrtc::RtpCodecParameters {
            clock_rate: 8000,
            channels: 1,
            payload_type: 0,
            ..Default::default()
        },
    )?;
    let offer = pc.create_offer().await?;
    pc.set_local_description(offer.clone())?;
    ws.send(Message::Text(
        serde_json::json!({"command": "invite", "option": {"offer": offer.to_sdp_string()}})
            .to_string()
            .into(),
    ))
    .await?;

    let answer = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(text) = msg {
                if let Ok(SessionEvent::Answer { sdp, .. }) = serde_json::from_str(&text) {
                    return Some(sdp);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
    .expect("no answer event");
    let answer = match relay {
        Some(relay) => relay.answer(&answer)?,
        None => answer,
    };
    pc.set_remote_description(rustrtc::SessionDescription::parse(
        rustrtc::SdpType::Answer,
        &answer,
    )?)
    .await?;
    tokio::time::timeout(Duration::from_secs(10), pc.wait_for_connected())
        .await
        .expect("client never connected")?;
    Ok((ws, pc, record_rx))
}

#[tokio::test]
async fn webrtc_call_hangs_up_when_connection_is_lost() -> Result<()> {
    let (_ws, pc, mut records) = connected_call("test-webrtc-connection-lost", "1s", None).await?;

    // the far end goes away without a word
    pc.close();
    drop(pc);

    let record = tokio::time::timeout(Duration::from_secs(45), records.recv())
        .await
        .expect("call stayed up after its media connection was lost")
        .expect("call record channel closed");
    assert_eq!(record.hangup_reason, Some(CallRecordHangupReason::Failed));
    Ok(())
}

#[tokio::test]
async fn webrtc_hangup_command_is_not_reported_as_connection_loss() -> Result<()> {
    let (mut ws, _pc, mut records) =
        connected_call("test-webrtc-normal-hangup", "1s", None).await?;

    ws.send(Message::Text(
        serde_json::json!({"command": "hangup", "reason": "caller"})
            .to_string()
            .into(),
    ))
    .await?;

    let record = tokio::time::timeout(Duration::from_secs(10), records.recv())
        .await
        .expect("no call record after the hangup command")
        .expect("call record channel closed");
    assert_eq!(record.hangup_reason, Some(CallRecordHangupReason::ByCaller));
    Ok(())
}

#[tokio::test]
async fn webrtc_call_survives_a_recovered_ice_disconnect() -> Result<()> {
    let relay = Relay::new().await?;
    let (_ws, pc, mut records) =
        connected_call("test-webrtc-ice-blip", "10s", Some(&relay)).await?;
    let mut state = pc.subscribe_peer_state();

    // rustrtc calls a path Disconnected after 30s of silence; the client
    // seeing it means the node, as silent, does too
    relay.set_blackout(true);
    tokio::time::timeout(
        Duration::from_secs(60),
        state.wait_for(|s| *s == PeerConnectionState::Disconnected),
    )
    .await
    .expect("client never saw the blackout")?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    relay.set_blackout(false);
    tokio::time::timeout(
        Duration::from_secs(5),
        state.wait_for(|s| *s == PeerConnectionState::Connected),
    )
    .await
    .expect("client never recovered")?;

    // well past the grace that started with the blackout
    let record = tokio::time::timeout(Duration::from_secs(15), records.recv()).await;
    assert!(
        record.is_err(),
        "call hung up although its connection recovered: {record:?}"
    );
    // and its media was not torn down without a hangup either
    assert_eq!(*state.borrow(), PeerConnectionState::Connected);
    Ok(())
}
