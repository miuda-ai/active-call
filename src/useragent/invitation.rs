use std::sync::Arc;

use crate::{
    call::{RoutingState, sip::Invitation, sip::remove_dialog},
    config::InviteHandlerConfig,
    useragent::{playbook_handler::PlaybookInvitationHandler, webhook::WebhookInvitationHandler},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsipstack::dialog::{
    DialogId,
    dialog::{Dialog, DialogStateReceiver},
    invite_dialog::InviteDialog,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct PendingDialog {
    pub token: CancellationToken,
    pub dialog: InviteDialog,
    pub state_receiver: DialogStateReceiver,
}
pub struct PendingDialogGuard {
    pub id: DialogId,
    /// Short public session id registered for this dialog; removed on drop.
    pub session_id: Option<String>,
    pub invitation: Invitation,
}

impl PendingDialogGuard {
    pub fn new(invitation: Invitation, id: DialogId, pending_dialog: PendingDialog) -> Self {
        invitation.add_pending(id.clone(), pending_dialog);
        info!(%id, "added pending dialog");
        Self {
            id,
            session_id: None,
            invitation,
        }
    }

    pub fn new_with_session(
        invitation: Invitation,
        id: DialogId,
        session_id: String,
        pending_dialog: PendingDialog,
    ) -> Self {
        invitation.add_pending(id.clone(), pending_dialog);
        invitation.register_session(&session_id, &id);
        info!(%id, %session_id, "added pending dialog");
        Self {
            id,
            session_id: Some(session_id),
            invitation,
        }
    }

    fn take_dialog(&self) -> Option<Dialog> {
        let pending = self.invitation.get_pending_call(&self.id)?;
        let dialog_id = pending.dialog.id();
        remove_dialog(&self.invitation.dialog_layer, &dialog_id)
    }
    pub async fn drop_async(&self) {
        if let Some(session_id) = &self.session_id {
            self.invitation.unregister_session(session_id);
        }
        if let Some(dialog) = self.take_dialog() {
            dialog.hangup().await.ok();
        }
    }
}

impl Drop for PendingDialogGuard {
    fn drop(&mut self) {
        if let Some(session_id) = &self.session_id {
            self.invitation.unregister_session(session_id);
        }
        if let Some(dialog) = self.take_dialog() {
            info!(%self.id, "removing pending dialog on drop");

            crate::spawn(async move {
                dialog.hangup().await.ok();
            });
        }
    }
}

#[async_trait]
pub trait InvitationHandler: Send + Sync {
    async fn on_invite(
        &self,
        _session_id: String,
        _cancel_token: CancellationToken,
        _dialog: InviteDialog,
        _routing_state: Arc<RoutingState>,
    ) -> Result<()> {
        return Err(anyhow!("invite not handled"));
    }
}

pub fn default_create_invite_handler(
    config: Option<&InviteHandlerConfig>,
    app_state: Option<crate::app::AppState>,
) -> Option<Box<dyn InvitationHandler>> {
    match config {
        Some(InviteHandlerConfig::Webhook {
            url,
            urls,
            method,
            headers,
        }) => {
            let all_urls = if let Some(urls) = urls {
                urls.clone()
            } else if let Some(url) = url {
                vec![url.clone()]
            } else {
                vec![]
            };
            Some(Box::new(WebhookInvitationHandler::new(
                all_urls,
                method.clone(),
                headers.clone(),
            )))
        }
        Some(InviteHandlerConfig::Playbook { rules, default }) => {
            let app_state = match app_state {
                Some(s) => s,
                None => {
                    tracing::error!("app_state required for playbook invitation handler");
                    return None;
                }
            };
            let rules = rules.clone().unwrap_or_default();
            match PlaybookInvitationHandler::new(rules, default.clone(), app_state) {
                Ok(handler) => Some(Box::new(handler)),
                Err(e) => {
                    tracing::error!("failed to create playbook invitation handler: {}", e);
                    None
                }
            }
        }
        _ => None,
    }
}

pub type FnCreateInvitationHandler =
    fn(config: Option<&InviteHandlerConfig>) -> Result<Box<dyn InvitationHandler>>;
