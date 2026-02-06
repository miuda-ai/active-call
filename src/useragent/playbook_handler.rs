use crate::{
    app::AppState, call::RoutingState, config::PlaybookRule,
    useragent::invitation::InvitationHandler,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::server_dialog::ServerInviteDialog;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct PlaybookInvitationHandler {
    rules: Vec<CompiledPlaybookRule>,
    default: Option<String>,
    app_state: AppState,
}

struct CompiledPlaybookRule {
    caller: Option<Regex>,
    callee: Option<Regex>,
    playbook: String,
}

impl PlaybookInvitationHandler {
    pub fn new(
        rules: Vec<PlaybookRule>,
        default: Option<String>,
        app_state: AppState,
    ) -> Result<Self> {
        let mut compiled_rules = Vec::new();

        for rule in rules {
            let caller_regex = if let Some(pattern) = rule.caller {
                Some(
                    Regex::new(&pattern)
                        .map_err(|e| anyhow!("invalid caller regex '{}': {}", pattern, e))?,
                )
            } else {
                None
            };

            let callee_regex = if let Some(pattern) = rule.callee {
                Some(
                    Regex::new(&pattern)
                        .map_err(|e| anyhow!("invalid callee regex '{}': {}", pattern, e))?,
                )
            } else {
                None
            };

            compiled_rules.push(CompiledPlaybookRule {
                caller: caller_regex,
                callee: callee_regex,
                playbook: rule.playbook.clone(),
            });
        }

        Ok(Self {
            rules: compiled_rules,
            default,
            app_state,
        })
    }

    pub fn match_playbook(&self, caller: &str, callee: &str) -> Option<String> {
        for rule in &self.rules {
            let caller_matches = rule
                .caller
                .as_ref()
                .map(|r| r.is_match(caller))
                .unwrap_or(true);

            let callee_matches = rule
                .callee
                .as_ref()
                .map(|r| r.is_match(callee))
                .unwrap_or(true);

            if caller_matches && callee_matches {
                return Some(rule.playbook.clone());
            }
        }

        self.default.clone()
    }

    fn extract_custom_headers(
        headers: &rsip::Headers,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        let mut extras = std::collections::HashMap::new();
        for header in headers.iter() {
            if let rsip::Header::Other(name, value) = header {
                // Capture all custom headers, Playbook logic can filter them later using `sip.extract_headers` if needed
                extras.insert(
                    name.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
        }
        extras
    }
}

#[async_trait]
impl InvitationHandler for PlaybookInvitationHandler {
    async fn on_invite(
        &self,
        dialog_id: String,
        cancel_token: CancellationToken,
        dialog: ServerInviteDialog,
        _routing_state: Arc<RoutingState>,
    ) -> Result<()> {
        let invite_request = dialog.initial_request();
        let caller = invite_request.from_header()?.uri()?.to_string();
        let callee = invite_request.to_header()?.uri()?.to_string();

        match self.match_playbook(&caller, &callee) {
            Some(playbook) => {
                info!(
                    dialog_id,
                    caller, callee, playbook, "matched playbook for invite"
                );

                // Extract custom headers
                let extras = Self::extract_custom_headers(&invite_request.headers);

                if !extras.is_empty() {
                    let mut params = self.app_state.pending_params.lock().await;
                    params.insert(dialog_id.clone(), extras);
                }

                // Store the playbook name in pending_playbooks
                {
                    let mut pending = self.app_state.pending_playbooks.lock().await;
                    pending.insert(dialog_id.clone(), playbook);
                }

                // Start call handler in background task
                let app_state = self.app_state.clone();
                let session_id = dialog_id.clone();
                let cancel_token_clone = cancel_token.clone();

                crate::spawn(async move {
                    use crate::call::{ActiveCallType, Command};
                    use bytes::Bytes;
                    use std::path::PathBuf;

                    // Pre-validate playbook file exists (for SIP calls)
                    // Remove from pending to get the playbook name
                    let playbook_name = {
                        let pending = app_state.pending_playbooks.lock().await;
                        pending.get(&session_id).cloned()
                    };

                    if let Some(name_or_content) = playbook_name {
                        if !name_or_content.trim().starts_with("---") {
                            // It's a file path, check if it exists
                            let path = if name_or_content.starts_with("config/playbook/") {
                                PathBuf::from(&name_or_content)
                            } else {
                                PathBuf::from("config/playbook").join(&name_or_content)
                            };

                            if !path.exists() {
                                warn!(session_id, path=?path, "Playbook file not found, rejecting SIP call");
                                // Reject the SIP dialog with 503
                                if let Err(e) = dialog.reject(
                                    Some(rsip::StatusCode::ServiceUnavailable),
                                    Some("Playbook Not Found".to_string()),
                                ) {
                                    warn!(session_id, "Failed to reject SIP dialog: {}", e);
                                }
                                // Clean up pending playbook
                                app_state.pending_playbooks.lock().await.remove(&session_id);
                                return;
                            }
                        }
                    }

                    let (_audio_sender, audio_receiver) =
                        tokio::sync::mpsc::unbounded_channel::<Bytes>();
                    let (command_sender, command_receiver) =
                        tokio::sync::mpsc::unbounded_channel::<Command>();
                    let (event_sender, _event_receiver) =
                        tokio::sync::mpsc::unbounded_channel::<crate::event::SessionEvent>();

                    // Don't accept dialog here - let ActiveCall handle it after creating the track
                    // This ensures proper SDP answer is generated

                    // Send Accept command immediately to trigger SDP negotiation
                    // This must be done before call_handler_core consumes the receiver
                    if let Err(e) = command_sender.send(Command::Accept {
                        option: Default::default(),
                    }) {
                        warn!(session_id, "Failed to send accept command: {}", e);
                        return;
                    }

                    // Start call handler core
                    let handler_task = crate::spawn(crate::handler::handler::call_handler_core(
                        ActiveCallType::Sip,
                        session_id.clone(),
                        app_state.clone(),
                        cancel_token_clone.clone(),
                        audio_receiver,
                        None, // server_side_track
                        true, // dump_events
                        20,   // ping_interval
                        command_receiver,
                        event_sender.clone(),
                    ));

                    // Wait for call to complete or cancellation
                    tokio::select! {
                        _ = handler_task => {
                            info!(session_id, "SIP call handler completed");
                        }
                        _ = cancel_token_clone.cancelled() => {
                            info!(session_id, "SIP call cancelled");
                        }
                    }

                    // Attempt to retrieve custom headers for BYE
                    let headers = {
                        let mut params = app_state.pending_params.lock().await;
                        // remove returns the map of extras for this session
                        if let Some(extras) = params.remove(&session_id) {
                            if let Some(h_val) = extras.get("_sip_headers") {
                                if let Ok(h_map) = serde_json::from_value::<
                                    std::collections::HashMap<String, String>,
                                >(h_val.clone())
                                {
                                    Some(h_map)
                                } else if let serde_json::Value::String(s) = h_val {
                                    // Handle case where set_var stores it as a string
                                    serde_json::from_str::<std::collections::HashMap<String, String>>(s).ok()
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };

                    let sip_headers = headers.map(|h_map| {
                        h_map
                            .into_iter()
                            .map(|(k, v)| rsip::Header::Other(k.into(), v.into()))
                            .collect::<Vec<_>>()
                    });

                    // Terminate the SIP dialog
                    if let Err(e) = dialog.bye_with_headers(sip_headers).await {
                        warn!(session_id, "Failed to send BYE: {}", e);
                    }
                });

                Ok(())
            }
            None => {
                warn!(
                    dialog_id,
                    caller, callee, "no playbook matched for invite, rejecting"
                );
                Err(anyhow!(
                    "no matching playbook found for caller {} and callee {}",
                    caller,
                    callee
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PlaybookRule;

    // Simpler helper that creates just the matching function for testing
    struct TestMatcher {
        rules: Vec<(Option<Regex>, Option<Regex>, String)>,
        default: Option<String>,
    }

    impl TestMatcher {
        fn new(rules: Vec<PlaybookRule>, default: Option<String>) -> Result<Self> {
            let mut compiled_rules = Vec::new();

            for rule in rules {
                let caller_regex = if let Some(pattern) = rule.caller {
                    Some(
                        Regex::new(&pattern)
                            .map_err(|e| anyhow!("invalid caller regex '{}': {}", pattern, e))?,
                    )
                } else {
                    None
                };

                let callee_regex = if let Some(pattern) = rule.callee {
                    Some(
                        Regex::new(&pattern)
                            .map_err(|e| anyhow!("invalid callee regex '{}': {}", pattern, e))?,
                    )
                } else {
                    None
                };

                compiled_rules.push((caller_regex, callee_regex, rule.playbook.clone()));
            }

            Ok(Self {
                rules: compiled_rules,
                default,
            })
        }

        fn match_playbook(&self, caller: &str, callee: &str) -> Option<String> {
            for (caller_re, callee_re, playbook) in &self.rules {
                let caller_matches = caller_re
                    .as_ref()
                    .map(|r| r.is_match(caller))
                    .unwrap_or(true);

                let callee_matches = callee_re
                    .as_ref()
                    .map(|r| r.is_match(callee))
                    .unwrap_or(true);

                if caller_matches && callee_matches {
                    return Some(playbook.clone());
                }
            }

            self.default.clone()
        }
    }

    #[test]
    fn test_playbook_rule_matching() {
        let rules = vec![
            PlaybookRule {
                caller: Some(r"^\+1\d{10}$".to_string()),
                callee: Some(r"^sip:support@.*".to_string()),
                playbook: "support.md".to_string(),
            },
            PlaybookRule {
                caller: Some(r"^\+86\d+$".to_string()),
                callee: None,
                playbook: "chinese.md".to_string(),
            },
            PlaybookRule {
                caller: None,
                callee: Some(r"^sip:sales@.*".to_string()),
                playbook: "sales.md".to_string(),
            },
        ];

        let matcher = TestMatcher::new(rules, Some("default.md".to_string())).unwrap();

        // Test US number to support
        assert_eq!(
            matcher.match_playbook("+12125551234", "sip:support@example.com"),
            Some("support.md".to_string())
        );

        // Test Chinese number (matches second rule)
        assert_eq!(
            matcher.match_playbook("+8613800138000", "sip:any@example.com"),
            Some("chinese.md".to_string())
        );

        // Test sales callee (matches third rule)
        assert_eq!(
            matcher.match_playbook("+44123456789", "sip:sales@example.com"),
            Some("sales.md".to_string())
        );

        // Test no match - should use default
        assert_eq!(
            matcher.match_playbook("+44123456789", "sip:other@example.com"),
            Some("default.md".to_string())
        );
    }

    #[test]
    fn test_playbook_rule_no_default() {
        let rules = vec![PlaybookRule {
            caller: Some(r"^\+1.*".to_string()),
            callee: None,
            playbook: "us.md".to_string(),
        }];

        let matcher = TestMatcher::new(rules, None).unwrap();

        // Matches
        assert_eq!(
            matcher.match_playbook("+12125551234", "sip:any@example.com"),
            Some("us.md".to_string())
        );

        // No match and no default
        assert_eq!(
            matcher.match_playbook("+44123456789", "sip:any@example.com"),
            None
        );
    }

    #[test]
    fn test_invalid_regex() {
        let rules = vec![PlaybookRule {
            caller: Some(r"[invalid(".to_string()),
            callee: None,
            playbook: "test.md".to_string(),
        }];

        let result = TestMatcher::new(rules, None);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("invalid caller regex"));
    }

    #[test]
    fn test_extract_custom_headers() {
        use rsip::Header;

        let mut headers = rsip::Headers::default();
        headers.push(Header::ContentLength(10.into())); // Standard header (ignored)
        headers.push(Header::Other("X-Tenant-ID".into(), "123".into()));
        headers.push(Header::Other("Custom-Header".into(), "xyz".into()));

        let extras = PlaybookInvitationHandler::extract_custom_headers(&headers);

        assert_eq!(extras.len(), 2);
        assert_eq!(
            extras.get("X-Tenant-ID").unwrap(),
            &serde_json::Value::String("123".to_string())
        );
        assert_eq!(
            extras.get("Custom-Header").unwrap(),
            &serde_json::Value::String("xyz".to_string())
        );
    }
}
