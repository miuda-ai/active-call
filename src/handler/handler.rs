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
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

fn filter_headers(
    extras: &mut std::collections::HashMap<String, serde_json::Value>,
    allowed_headers: &[String],
) {
    extras.retain(|k, _| allowed_headers.iter().any(|h| h.eq_ignore_ascii_case(k)));
}

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

/// Core call handling logic that works with either WebSocket or mpsc channels
pub async fn call_handler_core(
    call_type: ActiveCallType,
    session_id: String,
    app_state: AppState,
    cancel_token: CancellationToken,
    audio_receiver: tokio::sync::mpsc::UnboundedReceiver<Bytes>,
    server_side_track: Option<String>,
    dump_events: bool,
    ping_interval: u64,
    mut command_receiver: tokio::sync::mpsc::UnboundedReceiver<Command>,
    event_sender_to_client: tokio::sync::mpsc::UnboundedSender<crate::event::SessionEvent>,
) {
    let _cancel_guard = cancel_token.clone().drop_guard();
    let track_config = TrackConfig::default();

    // Check for pending params (extracted headers)
    let extras = {
        let mut pending = app_state.pending_params.lock().await;
        pending.remove(&session_id)
    };

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
        extras,
        None,
    ));

    // Check for pending playbook
    {
        let mut pending = app_state.pending_playbooks.lock().await;
        if let Some(name_or_content) = pending.remove(&session_id) {
            let playbook_result = if name_or_content.trim().starts_with("---") {
                Playbook::parse(&name_or_content)
            } else {
                // If path already contains config/playbook, use it as-is; otherwise prepend it
                let path = if name_or_content.starts_with("config/playbook/") {
                    PathBuf::from(&name_or_content)
                } else {
                    PathBuf::from("config/playbook").join(&name_or_content)
                };
                Playbook::load(path).await
            };

            match playbook_result {
                Ok(mut playbook) => {
                    // Filter extracted headers if configured (only for SIP calls)
                    if call_type == ActiveCallType::Sip {
                        if let Some(sip_config) = &playbook.config.sip {
                            if let Some(allowed_headers) = &sip_config.extract_headers {
                                let mut state = active_call.call_state.write().await;
                                if let Some(extras) = &mut state.extras {
                                    filter_headers(extras, allowed_headers);
                                    // Store the list of SIP header keys for later template rendering
                                    let header_keys: Vec<String> = extras
                                        .keys()
                                        .filter(|k| !k.starts_with('_'))
                                        .cloned()
                                        .collect();
                                    extras.insert(
                                        "_sip_header_keys".to_string(),
                                        serde_json::to_value(&header_keys).unwrap_or_default(),
                                    );
                                    if let Ok(result) = playbook.render(extras) {
                                        playbook = result;
                                    }
                                }
                            }
                        }
                    }

                    match PlaybookRunner::new(playbook, active_call.clone()) {
                        Ok(runner) => {
                            crate::spawn(async move {
                                runner.run().await;
                            });
                            let display_name = if name_or_content.trim().starts_with("---") {
                                "custom content"
                            } else {
                                &name_or_content
                            };
                            info!(session_id, "Playbook runner started for {}", display_name);
                        }
                        Err(e) => {
                            let display_name = if name_or_content.trim().starts_with("---") {
                                "custom content"
                            } else {
                                &name_or_content
                            };
                            warn!(
                                session_id,
                                "Failed to create runner {}: {}", display_name, e
                            )
                        }
                    }
                }
                Err(e) => {
                    let display_name = if name_or_content.trim().starts_with("---") {
                        "custom content"
                    } else {
                        &name_or_content
                    };
                    warn!(
                        session_id,
                        "Failed to load playbook {}: {}", display_name, e
                    );
                    let event = SessionEvent::Error {
                        timestamp: crate::media::get_timestamp(),
                        track_id: session_id.clone(),
                        sender: "playbook".to_string(),
                        error: format!("{}", e),
                        code: None,
                    };
                    event_sender_to_client.send(event).ok();
                    return;
                }
            }
        }
    }

    let recv_commands_loop = async {
        while let Some(command) = command_receiver.recv().await {
            if let Err(_) = active_call.enqueue_command(command).await {
                break;
            }
        }
    };

    let mut event_receiver = active_call.event_sender.subscribe();
    let send_events_loop = async {
        loop {
            match event_receiver.recv().await {
                Ok(event) => {
                    if let Err(_) = event_sender_to_client.send(event) {
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
        let mut ticker = tokio::time::interval(Duration::from_secs(ping_interval));
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
                _ = send_events_loop => { },
                _ = recv_commands_loop => {
                    info!(session_id, "Command receiver closed");
                },
            }
            cancel_token.cancel();
        }
    };
    match r {
        Ok(_) => info!(session_id, "call ended successfully"),
        Err(e) => warn!(session_id, "call ended with error: {}", e),
    }

    // Write back final extras to pending_params for post-call processing (e.g. BYE headers)
    if call_type == ActiveCallType::Sip {
        let state = active_call.call_state.read().await;
        if let Some(extras) = &state.extras {
            let mut pending = app_state.pending_params.lock().await;
            pending.insert(session_id.clone(), extras.clone());
        }
    }

    active_call.cleanup().await.ok();
    debug!(session_id, "Call handler core completed");
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
        let (command_sender, command_receiver) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let (event_sender_to_client, mut event_receiver_from_core) =
            tokio::sync::mpsc::unbounded_channel::<crate::event::SessionEvent>();
        let cancel_token = CancellationToken::new();

        // Start core handler in background
        let session_id_clone = session_id.clone();
        let app_state_clone = app_state.clone();
        let cancel_token_clone = cancel_token.clone();
        crate::spawn(call_handler_core(
            call_type,
            session_id_clone,
            app_state_clone,
            cancel_token_clone,
            audio_receiver,
            server_side_track,
            dump_events,
            ping_interval.into(),
            command_receiver,
            event_sender_to_client,
        ));

        // Handle WebSocket I/O
        let recv_from_ws_loop = async {
            while let Some(Ok(message)) = ws_receiver.next().await {
                match message {
                    Message::Text(text) => {
                        let command = match serde_json::from_str::<Command>(&text) {
                            Ok(cmd) => cmd,
                            Err(e) => {
                                warn!(session_id, %text, "Failed to parse command {}", e);
                                continue;
                            }
                        };
                        if let Err(_) = command_sender.send(command) {
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

        let send_to_ws_loop = async {
            while let Some(event) = event_receiver_from_core.recv().await {
                trace!(session_id, %event, "Sending WS message");
                let message = match event.into_ws_message() {
                    Ok(msg) => msg,
                    Err(e) => {
                        warn!(session_id, error=%e, "Failed to serialize event to WS message");
                        continue;
                    }
                };
                if let Err(_) = ws_sender.send(message).await {
                    info!(session_id, "WebSocket send failed, closing");
                    break;
                }
            }
        };

        select! {
            _ = recv_from_ws_loop => {
                info!(session_id, "WebSocket receive loop ended");
            },
            _ = send_to_ws_loop => {
                info!(session_id, "WebSocket send loop ended");
            },
            _ = cancel_token.cancelled() => {
                info!(session_id, "WebSocket cancelled");
            },
        }

        cancel_token.cancel();
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_filter_headers() {
        let mut extras = HashMap::new();
        extras.insert("X-Tenant-ID".to_string(), json!("123"));
        extras.insert("X-User-ID".to_string(), json!("456"));
        extras.insert("Custom-Header".to_string(), json!("abc"));
        extras.insert("Irrelevant-Header".to_string(), json!("ignore"));

        // Test case-insensitive matching
        let allowed = vec!["x-tenant-id".to_string(), "Custom-Header".to_string()];

        filter_headers(&mut extras, &allowed);

        assert!(extras.contains_key("X-Tenant-ID"));
        assert!(extras.contains_key("Custom-Header"));
        assert!(!extras.contains_key("X-User-ID"));
        assert!(!extras.contains_key("Irrelevant-Header"));

        // ensure values are preserved
        assert_eq!(extras.get("X-Tenant-ID").unwrap(), &json!("123"));
        assert_eq!(extras.get("Custom-Header").unwrap(), &json!("abc"));
    }
}
