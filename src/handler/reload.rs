//! Config hot-reload HTTP surface.
//!
//! ```text
//! POST /reload?mode=check   validate the file, report what would change
//! POST /reload?mode=apply   validate and install the new snapshot
//! GET  /config              effective config (secrets redacted) + revision
//! ```
//!
//! When `[reload] token = "..."` (or `PBX_RELOAD_TOKEN`) is set, every request
//! must carry it in `X-Reload-Token`; otherwise the routes are unauthenticated
//! and rely on the ops firewall, like rustpbx's `/reload/*` routes.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use crate::app::AppState;

#[derive(Debug, Deserialize)]
pub struct ReloadParams {
    /// `check`/`validate`/`dry_run` = do not install; `apply` (default) = install.
    pub mode: Option<String>,
}

pub fn reload_router() -> Router<AppState> {
    Router::new()
        .route("/reload", post(reload_handler))
        .route("/config", get(config_handler))
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.config().reload_token().map(str::to_string) else {
        return true;
    };
    headers
        .get("x-reload-token")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false)
}

async fn reload_handler(
    State(state): State<AppState>,
    Query(params): Query<ReloadParams>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status": "error", "message": "invalid or missing X-Reload-Token"})),
        )
            .into_response();
    }

    let check_only = matches!(
        params.mode.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("check") | Some("validate") | Some("dry_run") | Some("dry-run")
    );

    let Some(path) = state.config_path().map(str::to_string) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "message": "no config path recorded at startup; start with --conf / with_config_metadata",
            })),
        )
            .into_response();
    };

    // Parse + env overrides exactly like startup.
    let proposed = match crate::config::Config::load(&path) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(path = %path, check_only, error = %e, "reload: config parse failed");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "status": "invalid",
                    "message": format!("{path}: {e}"),
                })),
            )
                .into_response();
        }
    };

    let report = if check_only {
        state.preview_config(proposed)
    } else {
        state.apply_config(proposed)
    };

    let body = json!({
        "status": report.status,
        "mode": if check_only { "check" } else { "apply" },
        "revision": report.revision,
        "path": path,
        "applied": report.applied,
        "deferred": report.deferred,
        "requires_restart": report.requires_restart,
    });

    if check_only {
        tracing::info!(path = %path, changed = ?report.applied, "reload: check ok");
    } else {
        tracing::info!(
            path = %path,
            applied = ?report.applied,
            deferred = ?report.deferred,
            requires_restart = ?report.requires_restart,
            "CONFIGRELOAD applied"
        );
    }

    Json(body).into_response()
}

async fn config_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status": "error", "message": "invalid or missing X-Reload-Token"})),
        )
            .into_response();
    }

    let config = state.config();
    let mut value = serde_json::to_value(&*config).unwrap_or_else(|_| json!({}));
    redact_secrets(&mut value);

    let switches = crate::runtime::module_switches();
    Json(json!({
        "path": state.config_path(),
        "config": value,
        "effective": {
            "admission": {
                "max_active_calls": config.admission_max_calls(),
                "wait_ms": config.admission_wait_ms(),
                "active_sessions": crate::call::active_call::ACTIVE_SESSIONS
                    .load(std::sync::atomic::Ordering::Relaxed),
            },
            "modules": {
                "denoise": switches.denoise,
                "vad": switches.vad,
                "eou": switches.eou,
                "ringback_detection": switches.ringback_detection,
                "sipflow": switches.sipflow,
                "playbooks": switches.playbooks,
            },
        },
    }))
    .into_response()
}

/// Blank out credential-looking string fields wherever they appear.
fn redact_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, slot) in map.iter_mut() {
                let k = key.to_ascii_lowercase();
                if k.contains("secret") || k.contains("password") || k.contains("token") {
                    *slot = json!("***");
                } else {
                    redact_secrets(slot);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;
    use serde_json::json;

    #[test]
    fn redacts_secrets_recursively() {
        let mut value = json!({
            "callrecord": {"secret_key": "abc", "bucket": "b"},
            "nested": [{"api_token": "t"}, {"plain": 1}],
        });
        redact_secrets(&mut value);
        assert_eq!(value["callrecord"]["secret_key"], "***");
        assert_eq!(value["callrecord"]["bucket"], "b");
        assert_eq!(value["nested"][0]["api_token"], "***");
        assert_eq!(value["nested"][1]["plain"], 1);
    }
}
