use anyhow::Result;
use rsip::{HostWithPort, Response, StatusCodeKind, Transport};
use rsipstack::{
    dialog::{authenticate::Credential, registration::Registration},
    rsip_ext::RsipResponseExt, transport::SipAddr,
};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::public_address::{normalize_transport, should_update_address};

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct UserCredential {
    pub username: String,
    pub password: String,
    pub realm: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct RegisterOption {
    pub server: String,
    pub username: String,
    pub display_name: Option<String>,
    pub disabled: Option<bool>,
    pub credential: Option<UserCredential>,
}

impl From<UserCredential> for Credential {
    fn from(val: UserCredential) -> Self {
        Credential {
            username: val.username,
            password: val.password,
            realm: val.realm,
        }
    }
}

impl RegisterOption {
    pub fn aor(&self) -> String {
        format!("{}@{}", self.username, self.server)
    }
}

pub struct RegistrationHandle {
    pub registration: Registration,
    pub option: RegisterOption,
    pub cancel_token: CancellationToken,
    pub start_time: Instant,
    pub last_update: Instant,
    pub last_response: Option<Response>,
}

impl RegistrationHandle {
    pub fn should_retry_registration_now(
        &self,
        local_addr: &SipAddr,
        current_contact_address: &HostWithPort,
        discovered_public_address: Option<&HostWithPort>,
    ) -> bool {
        if normalize_transport(local_addr.r#type.as_ref()) != Transport::Udp {
            return false;
        }

        discovered_public_address
            .is_some_and(|address| should_update_address(current_contact_address, address))
    }

    pub async fn do_register(
        &mut self,
        sip_server: &rsip::Uri,
        expires: Option<u32>,
        contact: &rsip::typed::Contact,
    ) -> Result<(u32, Option<HostWithPort>)> {
        self.registration.contact = Some(contact.clone());
        let resp = match self
            .registration
            .register(sip_server.clone(), expires)
            .await
            .map_err(|e| anyhow::anyhow!("Registration failed: {}", e))
        {
            Ok(resp) => resp,
            Err(e) => {
                warn!("registration failed: {}", e);
                return Err(anyhow::anyhow!("Registration failed: {}", e));
            }
        };
        debug!(
            user = self.option.aor(),
            "registration response: {:?}", resp
        );
        match resp.status_code().kind() {
            StatusCodeKind::Successful => {
                self.last_update = Instant::now();
                self.last_response = Some(resp);
                Ok((
                    self.registration.expires(),
                    self.registration.public_address.clone(),
                ))
            }
            _ => Err(anyhow::anyhow!("{:?}", resp.reason_phrase())),
        }
    }
}
