//! Integration tests for peer websocket forwarding: the `tunnel` relay and the
//! `try_forward` peer-polling logic.

use active_call::{
    app::AppStateBuilder,
    call::active_call::CallParams,
    config::Config,
    handler::peer::{try_forward, tunnel},
};
use axum::{
    Router, extract::WebSocketUpgrade, http::StatusCode, response::IntoResponse, routing::get,
};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// Start an axum server with `app`, return (join handle, bound port). The
/// listener is bound before spawning so the port is guaranteed ready.
async fn spawn_axum(app: Router) -> (tokio::task::JoinHandle<()>, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (handle, port)
}

/// A peer echo server: immediately sends a greeting, echoes text frames back
/// prefixed with `echo:`, and echoes binary frames back unchanged.
fn echo_peer_app() -> Router {
    Router::new().route(
        "/call",
        get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                let _ = socket
                    .send(axum::extract::ws::Message::text("greeting-from-peer"))
                    .await;
                while let Some(Ok(msg)) = socket.next().await {
                    let reply = match msg {
                        axum::extract::ws::Message::Text(t) => Some(
                            axum::extract::ws::Message::text(format!("echo:{}", t.as_str())),
                        ),
                        axum::extract::ws::Message::Binary(b) => {
                            Some(axum::extract::ws::Message::binary(b))
                        }
                        _ => None,
                    };
                    if let Some(reply) = reply {
                        if socket.send(reply).await.is_err() {
                            break;
                        }
                    }
                }
            })
        }),
    )
}

/// A peer server that rejects every request with 404 (leaf node, call absent).
fn missing_peer_app() -> Router {
    Router::new().route(
        "/call",
        get(|| async { (StatusCode::NOT_FOUND, "call not found").into_response() }),
    )
}

#[tokio::test]
async fn tunnel_relays_frames_bidirectionally() {
    let (_peer_task, peer_port) = spawn_axum(echo_peer_app()).await;
    let peer_url = std::sync::Arc::new(format!("ws://127.0.0.1:{peer_port}/call"));

    // Node-side tunnel server: connects to the peer and relays bidirectionally.
    let app = {
        let peer_url = peer_url.clone();
        Router::new().route(
            "/call",
            get(move |ws: WebSocketUpgrade| {
                let peer_url = peer_url.clone();
                async move {
                    ws.on_upgrade(move |socket| async move {
                        let (peer, _) = connect_async(peer_url.as_str())
                            .await
                            .expect("connect to peer");
                        tunnel(socket, peer).await;
                    })
                }
            }),
        )
    };
    let (_node_task, node_port) = spawn_axum(app).await;

    let node_url = format!("ws://127.0.0.1:{node_port}/call?id=t1");
    let (mut client, _) = connect_async(&node_url).await.expect("connect to node");

    // 1. Greeting pushed by the peer must reach the client (peer -> client).
    let greeting = client.next().await.expect("expected greeting").expect("ok");
    let Message::Text(t) = greeting else {
        panic!("expected text greeting, got {greeting:?}");
    };
    assert_eq!(t.as_str(), "greeting-from-peer");

    // 2. Client text must reach the peer and its echo must come back
    //    (client -> peer -> client).
    client
        .send(Message::Text("hello-node".to_string().into()))
        .await
        .unwrap();
    let echoed = client.next().await.expect("expected echo").expect("ok");
    let Message::Text(et) = echoed else {
        panic!("expected text echo, got {echoed:?}");
    };
    assert_eq!(et.as_str(), "echo:hello-node");

    // 3. Binary frames also relay.
    client
        .send(Message::Binary(vec![1, 2, 3, 4].into()))
        .await
        .unwrap();
    let bin = client
        .next()
        .await
        .expect("expected binary echo")
        .expect("ok");
    assert_eq!(bin, Message::Binary(vec![1, 2, 3, 4].into()));

    client.close(None).await.unwrap();
}

/// try_forward must skip peers that reject (404) and accept the first peer that
/// upgrades the connection.
#[tokio::test]
async fn try_forward_skips_rejecting_peer_and_accepts_hosting_peer() {
    let (_missing_task, port_missing) = spawn_axum(missing_peer_app()).await;
    let (_host_task, port_host) = spawn_axum(echo_peer_app()).await;

    let mut config = Config::default();
    config.addr = "127.0.0.1".to_string();
    config.udp_port = 0;
    config.media_cache_path = "./target/tmp_media_test".to_string();
    config.peers = vec![
        format!("127.0.0.1:{port_missing}"),
        format!("127.0.0.1:{port_host}"),
    ];
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .build()
        .await
        .expect("failed to build app state");

    let params = CallParams {
        id: Some("sess-1".to_string()),
        dump_events: Some(true),
        ping_interval: None,
        server_side_track: None,
        forward: None,
        visited: None,
    };

    let ws = try_forward(&app_state, "sess-1", &params).await;
    assert!(
        ws.is_some(),
        "expected a connected peer websocket from the hosting peer"
    );
}

/// try_forward returns None when every peer rejects the call.
#[tokio::test]
async fn try_forward_returns_none_when_all_peers_reject() {
    let (_missing_task, port_missing) = spawn_axum(missing_peer_app()).await;

    let mut config = Config::default();
    config.addr = "127.0.0.1".to_string();
    config.udp_port = 0;
    config.media_cache_path = "./target/tmp_media_test".to_string();
    config.peers = vec![format!("127.0.0.1:{port_missing}")];
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .build()
        .await
        .expect("failed to build app state");

    let params = CallParams {
        id: Some("sess-2".to_string()),
        dump_events: None,
        ping_interval: None,
        server_side_track: None,
        forward: None,
        visited: None,
    };

    assert!(
        try_forward(&app_state, "sess-2", &params).await.is_none(),
        "expected None when no peer hosts the call"
    );
}

/// `forward` must be empty to hop. Any present value (`true` or `false`) is
/// local-only, even when a peer would accept the websocket.
#[tokio::test]
async fn try_forward_only_when_forward_is_empty() {
    let (_host_task, port_host) = spawn_axum(echo_peer_app()).await;

    let mut config = Config::default();
    config.addr = "127.0.0.1".to_string();
    config.udp_port = 0;
    config.http_addr = "0.0.0.0:8080".to_string();
    config.media_cache_path = "./target/tmp_media_test".to_string();
    config.peers = vec![format!("127.0.0.1:{port_host}")];
    let app_state = AppStateBuilder::new()
        .with_config(config)
        .build()
        .await
        .expect("failed to build app state");

    for forward in [Some(true), Some(false)] {
        let params = CallParams {
            id: Some("sess-nested".to_string()),
            dump_events: None,
            ping_interval: None,
            server_side_track: None,
            forward,
            visited: None,
        };
        assert!(
            try_forward(&app_state, "sess-nested", &params)
                .await
                .is_none(),
            "forward={forward:?} must not hop to another peer"
        );
    }
}

/// Helper: build a full AppState-backed call router with no peers configured.
async fn local_node_app_state() -> active_call::app::AppState {
    let mut config = Config::default();
    config.addr = "127.0.0.1".to_string();
    config.udp_port = 0;
    config.http_addr = "0.0.0.0:8080".to_string();
    config.media_cache_path = "./target/tmp_media_test".to_string();
    AppStateBuilder::new()
        .with_config(config)
        .build()
        .await
        .expect("failed to build app state")
}

/// A node receiving `forward=true` for a session it does not host must answer
/// 404 (so the originator polls its next peer) and must NOT create the call.
#[tokio::test]
async fn forward_true_gets_404_and_does_not_create_call() {
    let app_state = local_node_app_state().await;
    let router = active_call::handler::call_router().with_state(app_state.clone());
    let (_task, port) = spawn_axum(router).await;

    let url = format!("ws://127.0.0.1:{port}/call?id=missing-session&forward=true");
    let err = connect_async(url)
        .await
        .expect_err("forward=true with absent session must fail the handshake");

    let status = match &err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => resp.status(),
        other => panic!("expected HTTP error, got {other:?}"),
    };
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert!(
        app_state.active_calls.lock().unwrap().is_empty(),
        "forward=true must not create a call locally"
    );
}

/// Without any `forward` param and no peers configured, the node must fall
/// back to creating the call locally (pre-forwarding behaviour preserved).
#[tokio::test]
async fn no_forward_creates_call_locally_when_no_peers() {
    let app_state = local_node_app_state().await;
    let router = active_call::handler::call_router().with_state(app_state.clone());
    let (_task, port) = spawn_axum(router).await;

    let url = format!("ws://127.0.0.1:{port}/call?id=local-session");
    let (_ws, _resp) = connect_async(url)
        .await
        .expect("local call must be created");

    // The call must be registered once the upgrade succeeded.
    let mut registered = false;
    for _ in 0..50 {
        if app_state
            .active_calls
            .lock()
            .unwrap()
            .contains_key("local-session")
        {
            registered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(registered, "call must be registered in active_calls");
}
