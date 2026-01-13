use crate::{
    app::AppState,
    call::{
        ActiveCall, ActiveCallType, Command,
        active_call::{ActiveCallGuard, CallParams},
    },
    handler::playbook,
    playbook::{Playbook, PlaybookRunner},
};
use crate::{event::SessionEvent, media::track::TrackConfig};
use axum::{
    Json, Router,
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use rustrtc::IceServer;
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{join, select};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub fn call_router() -> Router<AppState> {
    let r = Router::new()
        .route("/call", get(ws_handler))
        .route("/call/webrtc", get(webrtc_handler))
        .route("/call/sip", get(sip_handler))
        .route("/list", get(list_active_calls))
        .route("/kill/{id}", get(kill_active_call));
    r
}

pub fn iceservers_router() -> Router<AppState> {
    let r = Router::new();
    r.route("/iceservers", get(get_iceservers))
}

pub fn playbook_router() -> Router<AppState> {
    Router::new()
        .route("/api/playbooks", get(playbook::list_playbooks))
        .route(
            "/api/playbooks/{name}",
            get(playbook::get_playbook).post(playbook::save_playbook),
        )
        .route(
            "/api/playbook/run",
            axum::routing::post(playbook::run_playbook),
        )
        .route("/api/records", get(playbook::list_records))
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(ActiveCallType::WebSocket, ws, state, params).await
}

pub async fn sip_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(ActiveCallType::Sip, ws, state, params).await
}

pub async fn webrtc_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(ActiveCallType::Webrtc, ws, state, params).await
}

pub async fn call_handler(
    call_type: ActiveCallType,
    ws: WebSocketUpgrade,
    app_state: AppState,
    params: CallParams,
) -> Response {
    let session_id = params
        .id
        .unwrap_or_else(|| format!("s.{}", Uuid::new_v4().to_string()));
    let server_side_track = params.server_side_track.clone();
    let dump_events = params.dump_events.unwrap_or(true);
    let ping_interval = params.ping_interval.unwrap_or(20);

    let resp = ws.on_upgrade(move |socket| async move {
        let (mut ws_sender, mut ws_receiver) = socket.split();
        let (audio_sender, audio_receiver) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        let cancel_token = CancellationToken::new();
        let track_config = TrackConfig::default();
        let active_call = Arc::new(ActiveCall::new(
            call_type.clone(),
            cancel_token.clone(),
            session_id.clone(),
            app_state.invitation.clone(),
            app_state.clone(),
            track_config,
            Some(audio_receiver),
            dump_events,
            server_side_track,
            None, // No extra data for now
        ));

        // Check for pending playbook
        {
            let mut pending = app_state.pending_playbooks.lock().await;
            if let Some(name) = pending.remove(&session_id) {
                let path = PathBuf::from("config/playbook").join(&name);
                let variables = active_call.call_state.read().await.extras.clone();
                match Playbook::load(path, variables.as_ref()).await {
                    Ok(playbook) => match PlaybookRunner::new(playbook, active_call.clone()) {
                        Ok(runner) => {
                            tokio::spawn(async move {
                                runner.run().await;
                            });
                            info!(session_id, "Playbook runner started for {}", name);
                        }
                        Err(e) => warn!(session_id, "Failed to create runner {}: {}", name, e),
                    },
                    Err(e) => {
                        warn!(session_id, "Failed to load playbook {}: {}", name, e);
                        let event = SessionEvent::Error {
                            timestamp: crate::media::get_timestamp(),
                            track_id: session_id,
                            sender: "playbook".to_string(),
                            error: format!("{}", e),
                            code: None,
                        };
                        let message = match event.into_ws_message() {
                            Ok(msg) => msg,
                            Err(_) => return,
                        };
                        ws_sender.send(message).await.ok();
                        return;
                    }
                }
            }
        }

        let recv_from_ws_loop = async {
            while let Some(Ok(message)) = ws_receiver.next().await {
                match message {
                    Message::Text(text) => {
                        let command = match serde_json::from_str::<Command>(&text) {
                            Ok(cmd) => cmd,
                            Err(e) => {
                                warn!(session_id, %text, "Failed to parse command {}",e);
                                continue;
                            }
                        };
                        if let Err(_) = active_call.enqueue_command(command).await {
                            break;
                        }
                    }
                    Message::Binary(bin) => {
                        audio_sender.send(bin.into()).ok();
                    }
                    Message::Close(_) => {
                        info!(session_id, "WebSocket closed by client");
                        break;
                    }
                    _ => {}
                }
            }
        };

        let mut event_receiver = active_call.event_sender.subscribe();
        let send_to_ws_loop = async {
            loop {
                match event_receiver.recv().await {
                    Ok(event) => {
                        let message = match event.into_ws_message() {
                            Ok(msg) => msg,
                            Err(_) => continue,
                        };
                        if let Err(_) = ws_sender.send(message).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        };

        let send_ping_loop = async {
            if ping_interval == 0 {
                active_call.cancel_token.cancelled().await;
                return;
            }
            let mut ticker = tokio::time::interval(Duration::from_secs(ping_interval.into()));
            loop {
                ticker.tick().await;
                let payload = Utc::now().to_rfc3339();
                let event = SessionEvent::Ping {
                    timestamp: crate::media::get_timestamp(),
                    payload: Some(payload),
                };
                if let Err(_) = active_call.event_sender.send(event) {
                    break;
                }
            }
        };
        let guard = ActiveCallGuard::new(active_call.clone());
        info!(
            session_id,
            active_calls = guard.active_calls,
            ?call_type,
            "new call started"
        );
        let receiver = active_call.new_receiver();

        let (r, _) = join! {
            active_call.serve(receiver),
            async {
                select!{
                    _ = send_ping_loop => {},
                    _ = cancel_token.cancelled() => {},
                    _ = send_to_ws_loop => { },
                    _ = recv_from_ws_loop => {
                        info!(session_id, "WebSocket closed by client");
                    },
                }
                cancel_token.cancel();
            }
        };
        match r {
            Ok(_) => info!(session_id, "call ended successfully"),
            Err(e) => warn!(session_id, "call ended with error: {}", e),
        }

        active_call.cleanup().await.ok();
        // Drain remaining events
        while let Ok(event) = event_receiver.try_recv() {
            let message = match event.into_ws_message() {
                Ok(msg) => msg,
                Err(_) => continue,
            };
            if let Err(_) = ws_sender.send(message).await {
                break;
            }
        }
        ws_sender.flush().await.ok();
        ws_sender.close().await.ok();
        debug!(session_id, "WebSocket connection closed");
    });
    resp
}

pub(crate) async fn get_iceservers(State(state): State<AppState>) -> Response {
    if let Some(ice_servers) = state.config.ice_servers.as_ref() {
        return Json(ice_servers).into_response();
    }
    Json(vec![IceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_string()],
        ..Default::default()
    }])
    .into_response()
}

pub(crate) async fn list_active_calls(State(state): State<AppState>) -> Response {
    let calls = state
        .active_calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, c)| {
            if let Ok(cs) = c.call_state.try_read() {
                json!({
                    "id": c.session_id,
                    "callType": c.call_type,
                    "cs.option": cs.option,
                    "ringTime": cs.ring_time,
                    "startTime": cs.answer_time,
                })
            } else {
                json!({
                    "id": c.session_id,
                    "callType": c.call_type,
                    "status": "locked",
                })
            }
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "active_calls": calls })).into_response()
}

pub(crate) async fn kill_active_call(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let active_calls = state.active_calls.lock().unwrap();
    if let Some(call) = active_calls.get(&id) {
        call.cancel_token.cancel();
        Json(serde_json::json!({ "status": "killed", "id": id })).into_response()
    } else {
        Json(serde_json::json!({ "status": "not_found", "id": id })).into_response()
    }
}

trait IntoWsMessage {
    fn into_ws_message(self) -> Result<Message, serde_json::Error>;
}

impl IntoWsMessage for crate::event::SessionEvent {
    fn into_ws_message(self) -> Result<Message, serde_json::Error> {
        match self {
            SessionEvent::Binary { data, .. } => Ok(Message::Binary(data.into())),
            SessionEvent::Ping { timestamp, payload } => {
                let payload = payload.unwrap_or_else(|| timestamp.to_string());
                Ok(Message::Ping(payload.into()))
            }
            event => serde_json::to_string(&event).map(|payload| Message::Text(payload.into())),
        }
    }
}
