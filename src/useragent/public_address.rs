use arc_swap::ArcSwap;
use rsip::{headers::ToTypedHeader, prelude::HeadersExt};
use rsipstack::{
    rsip_ext::RsipResponseExt, transaction::endpoint::MessageInspector, transport::SipAddr,
};
use std::{net::IpAddr, sync::Arc};

pub type SharedPublicAddress = Arc<ArcSwap<rsip::HostWithPort>>;

pub fn normalize_transport(transport: Option<&rsip::Transport>) -> rsip::Transport {
    transport.cloned().unwrap_or(rsip::Transport::Udp)
}

pub fn transport_for_uri(uri: &rsip::Uri) -> rsip::Transport {
    if matches!(uri.scheme, Some(rsip::Scheme::Sips)) {
        return rsip::Transport::Tls;
    }

    uri.params
        .iter()
        .find_map(|param| match param {
            rsip::Param::Transport(transport) => Some(transport.clone()),
            _ => None,
        })
        .unwrap_or(rsip::Transport::Udp)
}

pub fn find_local_addr_for_uri(addrs: &[SipAddr], uri: &rsip::Uri) -> Option<SipAddr> {
    let transport = transport_for_uri(uri);
    addrs.iter()
        .find(|addr| normalize_transport(addr.r#type.as_ref()) == transport)
        .cloned()
}

pub fn contact_needs_public_resolution(contact: &rsip::Uri) -> bool {
    if contact.scheme.is_none() {
        return true;
    }

    match &contact.host_with_port.host {
        rsip::Host::Domain(domain) => {
            let host = domain.to_string();
            host.eq_ignore_ascii_case("localhost")
        }
        rsip::Host::IpAddr(ip) => is_local_or_unspecified(ip),
    }
}

pub fn build_contact_uri(
    local_addr: &SipAddr,
    learned_addr: Option<rsip::HostWithPort>,
    username: Option<&str>,
    template: Option<&rsip::Uri>,
) -> rsip::Uri {
    let mut uri = template
        .cloned()
        .unwrap_or_else(|| rsip::Uri::from(local_addr));

    uri.host_with_port = learned_addr.unwrap_or_else(|| local_addr.addr.clone());
    if uri.scheme.is_none() {
        uri.scheme = Some(match local_addr.r#type {
            Some(rsip::Transport::Tls)
            | Some(rsip::Transport::Wss)
            | Some(rsip::Transport::TlsSctp) => rsip::Scheme::Sips,
            _ => rsip::Scheme::Sip,
        });
    }

    if uri.auth.is_none() {
        if let Some(username) = username.filter(|value| !value.is_empty()) {
            uri.auth = Some(rsip::Auth {
                user: username.to_string(),
                password: None,
            });
        }
    }

    uri
}

pub fn build_contact(
    local_addr: &SipAddr,
    contact_address: Option<rsip::HostWithPort>,
    username: Option<&str>,
    template: Option<&rsip::Uri>,
) -> rsip::typed::Contact {
    let contact_uri = build_contact_uri(local_addr, contact_address, username, template);
    rsip::typed::Contact {
        display_name: None,
        uri: contact_uri,
        params: vec![],
    }
}

pub fn build_public_contact_uri(
    learned_public_address: &SharedPublicAddress,
    auto_learn_public_address: bool,
    local_addr: &SipAddr,
    username: Option<&str>,
    template: Option<&rsip::Uri>,
) -> rsip::Uri {
    let selected_addr = if auto_learn_public_address
        && normalize_transport(local_addr.r#type.as_ref()) == rsip::Transport::Udp
    {
        Some(learned_public_address.load_full().as_ref().clone())
    } else {
        Some(local_addr.addr.clone())
    };
    build_contact_uri(local_addr, selected_addr, username, template)
}

pub struct LearningMessageInspector {
    learned_public_address: SharedPublicAddress,
    next: Option<Box<dyn MessageInspector>>,
}

impl LearningMessageInspector {
    pub fn new(
        initial_address: rsip::HostWithPort,
        next: Option<Box<dyn MessageInspector>>,
    ) -> Self {
        Self {
            learned_public_address: Arc::new(ArcSwap::from_pointee(initial_address)),
            next,
        }
    }

    pub fn shared_public_address(&self) -> SharedPublicAddress {
        self.learned_public_address.clone()
    }
}

impl MessageInspector for LearningMessageInspector {
    fn before_send(&self, msg: rsip::SipMessage, dest: Option<&SipAddr>) -> rsip::SipMessage {
        if let Some(next) = &self.next {
            next.before_send(msg, dest)
        } else {
            msg
        }
    }

    fn after_received(&self, msg: rsip::SipMessage, from: &SipAddr) -> rsip::SipMessage {
        if let rsip::SipMessage::Response(response) = &msg
            && let Ok(via) = response.via_header()
            && let Ok(via) = via.typed()
            && via.transport == rsip::Transport::Udp
            && let Some(host_with_port) = response.via_received()
        {
            self.learned_public_address
                .rcu(|previous: &Arc<rsip::HostWithPort>| {
                    if should_update_address(previous.as_ref(), &host_with_port) {
                        Arc::new(host_with_port.clone())
                    } else {
                        previous.clone()
                    }
                });
        }

        if let Some(next) = &self.next {
            next.after_received(msg, from)
        } else {
            msg
        }
    }
}

pub fn should_update_address(
    previous: &rsip::HostWithPort,
    current: &rsip::HostWithPort,
) -> bool {
    if previous == current {
        return false;
    }

    let previous_is_public = is_public_address(previous);
    let current_is_public = is_public_address(current);
    (!previous_is_public && current_is_public)
        || (previous_is_public && current_is_public && previous != current)
}

fn is_public_address(host_with_port: &rsip::HostWithPort) -> bool {
    match &host_with_port.host {
        rsip::Host::Domain(domain) => !domain.to_string().eq_ignore_ascii_case("localhost"),
        rsip::Host::IpAddr(ip) => !is_local_or_unspecified(ip),
    }
}

fn is_local_or_unspecified(ip: &IpAddr) -> bool {
    ip.is_loopback() || ip.is_unspecified()
}

#[cfg(test)]
mod tests {
    use super::{
        SharedPublicAddress, build_contact, build_contact_uri, build_public_contact_uri,
        contact_needs_public_resolution, find_local_addr_for_uri, should_update_address,
        transport_for_uri,
    };
    use arc_swap::ArcSwap;
    use rsip::transport::Transport;
    use rsipstack::transaction::endpoint::MessageInspector;
    use rsipstack::transport::SipAddr;
    use std::sync::Arc;

    #[test]
    fn learns_public_address_from_response_via() {
        let response: rsip::Response = concat!(
            "SIP/2.0 401 Unauthorized\r\n",
            "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1;received=203.0.113.10;rport=62000\r\n",
            "Content-Length: 0\r\n",
            "\r\n"
        )
        .try_into()
        .unwrap();

        let inspector = super::LearningMessageInspector::new(
            "127.0.0.1:5060"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
            None,
        );
        let cache = inspector.shared_public_address();
        inspector.after_received(
            rsip::SipMessage::Response(response),
            &SipAddr {
                r#type: Some(Transport::Udp),
                addr: "10.0.0.1:5060"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            },
        );
        assert_eq!(cache.load_full().as_ref().to_string(), "203.0.113.10:62000");
    }

    #[test]
    fn builds_contact_using_learned_public_address() {
        let local_addr = SipAddr {
            r#type: Some(Transport::Udp),
            addr: "10.0.0.5:5060"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        };
        let template: rsip::Uri = "sip:alice@127.0.0.1:5060".try_into().unwrap();
        let learned_addr = Some(
            "203.0.113.10:62000"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        );

        let contact = build_contact_uri(&local_addr, learned_addr, Some("alice"), Some(&template));
        assert_eq!(contact.to_string(), "sip:alice@203.0.113.10:62000");
    }

    #[test]
    fn identifies_contacts_that_need_resolution() {
        let local_contact: rsip::Uri = "sip:alice@127.0.0.1:5060".try_into().unwrap();
        let remote_contact: rsip::Uri = "sip:alice@203.0.113.10:62000".try_into().unwrap();
        assert!(contact_needs_public_resolution(&local_contact));
        assert!(!contact_needs_public_resolution(&remote_contact));
    }

    #[test]
    fn selects_local_addr_for_uri_transport() {
        let addrs = vec![
            SipAddr {
                r#type: Some(Transport::Udp),
                addr: "10.0.0.5:5060"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            },
            SipAddr {
                r#type: Some(Transport::Tls),
                addr: "10.0.0.5:5061"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            },
        ];

        let uri: rsip::Uri = "sips:alice@example.com".try_into().unwrap();
        let selected = find_local_addr_for_uri(&addrs, &uri).unwrap();

        assert_eq!(selected.to_string(), "TLS 10.0.0.5:5061");
    }

    #[test]
    fn builds_public_contact_from_shared_cache() {
        let cache: SharedPublicAddress = Arc::new(ArcSwap::from_pointee(
            "203.0.113.20:62000"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        ));
        let local_addr = SipAddr {
            r#type: Some(Transport::Udp),
            addr: "10.0.0.5:5060"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        };

        let contact = build_public_contact_uri(&cache, true, &local_addr, Some("alice"), None);
        assert_eq!(contact.to_string(), "sip:alice@203.0.113.20:62000");
    }

    #[test]
    fn builds_typed_contact() {
        let local_addr = SipAddr {
            r#type: Some(Transport::Udp),
            addr: "10.0.0.5:5060"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        };
        let contact = build_contact(
            &local_addr,
            Some(
                "203.0.113.20:62000"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            ),
            Some("alice"),
            None,
        );
        assert_eq!(contact.to_string(), "<sip:alice@203.0.113.20:62000>");
    }

    #[test]
    fn keeps_configured_contact_for_tls() {
        let cache: SharedPublicAddress = Arc::new(ArcSwap::from_pointee(
            "203.0.113.20:62000"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        ));
        let local_addr = SipAddr {
            r#type: Some(Transport::Tls),
            addr: "10.0.0.5:5061"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        };

        let contact = build_public_contact_uri(&cache, true, &local_addr, Some("alice"), None);
        assert_eq!(contact.to_string(), "sips:alice@10.0.0.5:5061;transport=TLS");
    }

    #[test]
    fn infers_transport_from_uri() {
        let sips_uri: rsip::Uri = "sips:alice@example.com".try_into().unwrap();
        let tcp_uri: rsip::Uri = "sip:alice@example.com;transport=tcp".try_into().unwrap();
        assert_eq!(transport_for_uri(&sips_uri), Transport::Tls);
        assert_eq!(transport_for_uri(&tcp_uri), Transport::Tcp);
    }

    #[test]
    fn updates_learned_address_from_local_to_public() {
        let previous: rsip::HostWithPort = "127.0.0.1:5060"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();
        let current: rsip::HostWithPort = "203.0.113.10:62000"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();

        assert!(should_update_address(&previous, &current,));
    }

    #[test]
    fn does_not_update_learned_address_when_unchanged() {
        let current: rsip::HostWithPort = "203.0.113.10:62000"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();

        assert!(!should_update_address(&current, &current,));
    }
}
