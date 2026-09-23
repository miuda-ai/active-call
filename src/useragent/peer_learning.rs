//! Learns the source addresses of peers we actually exchange call traffic
//! with, so OPTIONS keep-alive probes can be answered selectively (see
//! `[options_response]` in `Config`).
//!
//! Only call-bearing traffic is learned — inbound INVITE requests and
//! responses to our outbound INVITEs. Probes (OPTIONS) and registrations are
//! deliberately ignored: learning them would let an arbitrary scanner teach
//! the ACL to accept its own probes.

use lru::LruCache;
use rsipstack::rsip::prelude::HeadersExt;
use rsipstack::rsip::{Host, Method, SipMessage};
use rsipstack::transaction::endpoint::MessageInspector;
use std::collections::HashMap;
use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::trace;

/// Default number of peer entries kept in the LRU table.
pub const LEARNED_PEERS_CAPACITY: usize = 1024;

/// Cloneable handle to the LRU table of learned peer addresses
/// (peer IP -> instant of the last call traffic seen from it).
#[derive(Clone)]
pub struct SharedLearnedPeers {
    inner: Arc<Mutex<LruCache<IpAddr, Instant>>>,
}

impl SharedLearnedPeers {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity.max(1)).unwrap(),
            ))),
        }
    }

    /// Records `ip` as a recently active peer.
    pub fn learn(&self, ip: IpAddr) {
        if let Ok(mut cache) = self.inner.lock() {
            cache.put(ip, Instant::now());
        }
    }

    /// Whether `ip` was learned within `ttl`. Lookups do not refresh the LRU
    /// recency, so eviction order is driven purely by call traffic.
    pub fn contains_within(&self, ip: &IpAddr, ttl: Duration) -> bool {
        let Ok(cache) = self.inner.lock() else {
            return false;
        };
        match cache.peek(ip) {
            Some(learned_at) => learned_at.elapsed() <= ttl,
            None => false,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|cache| cache.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn snapshot(&self) -> HashMap<IpAddr, Instant> {
        self.inner
            .lock()
            .map(|cache| cache.iter().map(|(ip, at)| (*ip, *at)).collect())
            .unwrap_or_default()
    }
}

/// Transport-layer inspector recording call-traffic peer addresses.
pub struct PeerAddressLearner {
    peers: SharedLearnedPeers,
    next: Option<Box<dyn MessageInspector>>,
}

impl PeerAddressLearner {
    pub fn new(peers: SharedLearnedPeers) -> Self {
        Self { peers, next: None }
    }

    pub fn new_with_next(
        peers: SharedLearnedPeers,
        next: Option<Box<dyn MessageInspector>>,
    ) -> Self {
        Self { peers, next }
    }

    pub fn shared(&self) -> SharedLearnedPeers {
        self.peers.clone()
    }

    fn learn_from(&self, msg: &SipMessage, from: Option<&rsipstack::transport::SipAddr>) {
        let call_traffic = match msg {
            SipMessage::Request(req) => req.method == Method::Invite,
            SipMessage::Response(resp) => {
                resp.cseq_header().and_then(|cseq| cseq.method()) == Ok(Method::Invite)
            }
        };
        if !call_traffic {
            return;
        }
        let Some(from) = from else {
            return;
        };
        let Some(ip) = peer_ip(&from.addr) else {
            trace!(peer = %from.addr, "peer address is not an IP literal, not learned");
            return;
        };
        trace!(%ip, "learned peer address from call traffic");
        self.peers.learn(ip);
    }
}

fn peer_ip(host_with_port: &rsipstack::rsip::HostWithPort) -> Option<IpAddr> {
    match &host_with_port.host {
        Host::IpAddr(ip) => Some(*ip),
        Host::Domain(_) => None,
    }
}

impl MessageInspector for PeerAddressLearner {
    fn before_send(
        &self,
        msg: SipMessage,
        dest: Option<&rsipstack::transport::SipAddr>,
    ) -> SipMessage {
        match &self.next {
            Some(next) => next.before_send(msg, dest),
            None => msg,
        }
    }

    fn after_received(
        &self,
        msg: SipMessage,
        from: Option<&rsipstack::transport::SipAddr>,
    ) -> SipMessage {
        self.learn_from(&msg, from);
        match &self.next {
            Some(next) => next.after_received(msg, from),
            None => msg,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::rsip::{Header, Headers, HostWithPort, Request, Response, Uri, Version};
    use rsipstack::sip::Transport;
    use rsipstack::transport::SipAddr;

    fn udp_addr(ip: &str) -> SipAddr {
        SipAddr {
            addr: HostWithPort::try_from(format!("{ip}:5060")).unwrap(),
            r#type: Some(Transport::Udp),
        }
    }

    fn invite_request(call_id: &str) -> SipMessage {
        SipMessage::Request(Request {
            method: Method::Invite,
            uri: Uri::try_from("sip:bot@example.com").unwrap(),
            headers: Headers::from(vec![
                Header::CallId(call_id.into()),
                Header::CSeq("1 INVITE".into()),
            ]),
            version: Version::V2,
            body: vec![],
        })
    }

    fn options_request() -> SipMessage {
        SipMessage::Request(Request {
            method: Method::Options,
            uri: Uri::try_from("sip:bot@example.com").unwrap(),
            headers: Headers::from(vec![
                Header::CallId("probe".into()),
                Header::CSeq("10 OPTIONS".into()),
            ]),
            version: Version::V2,
            body: vec![],
        })
    }

    fn response_with_cseq(cseq: &str) -> SipMessage {
        SipMessage::Response(Response {
            headers: Headers::from(vec![Header::CSeq(cseq.into())]),
            ..Default::default()
        })
    }

    #[test]
    fn learns_only_call_traffic() {
        let learner = PeerAddressLearner::new(SharedLearnedPeers::new(16));

        learner.learn_from(&invite_request("c1"), Some(&udp_addr("1.1.1.1")));
        learner.learn_from(&response_with_cseq("1 INVITE"), Some(&udp_addr("2.2.2.2")));
        // OPTIONS probes and REGISTER traffic must not teach the table.
        learner.learn_from(&options_request(), Some(&udp_addr("3.3.3.3")));
        learner.learn_from(
            &response_with_cseq("1 REGISTER"),
            Some(&udp_addr("4.4.4.4")),
        );

        let ttl = Duration::from_secs(60);
        assert!(
            learner
                .shared()
                .contains_within(&"1.1.1.1".parse().unwrap(), ttl)
        );
        assert!(
            learner
                .shared()
                .contains_within(&"2.2.2.2".parse().unwrap(), ttl)
        );
        assert!(
            !learner
                .shared()
                .contains_within(&"3.3.3.3".parse().unwrap(), ttl)
        );
        assert!(
            !learner
                .shared()
                .contains_within(&"4.4.4.4".parse().unwrap(), ttl)
        );
    }

    #[test]
    fn ttl_expiry_and_lru_bound() {
        let learner = PeerAddressLearner::new(SharedLearnedPeers::new(2));

        learner.learn_from(&invite_request("c1"), Some(&udp_addr("10.0.0.1")));
        learner.learn_from(&invite_request("c2"), Some(&udp_addr("10.0.0.2")));
        // Third peer evicts the least recently used one (10.0.0.1).
        learner.learn_from(&invite_request("c3"), Some(&udp_addr("10.0.0.3")));

        let ttl = Duration::from_secs(3600);
        assert_eq!(learner.shared().len(), 2);
        assert!(
            !learner
                .shared()
                .contains_within(&"10.0.0.1".parse().unwrap(), ttl)
        );
        assert!(
            learner
                .shared()
                .contains_within(&"10.0.0.3".parse().unwrap(), ttl)
        );

        // A zero TTL expires everything even while still cached.
        assert!(
            !learner
                .shared()
                .contains_within(&"10.0.0.3".parse().unwrap(), Duration::ZERO)
        );
    }

    #[test]
    fn non_ip_peer_addresses_are_ignored() {
        let learner = PeerAddressLearner::new(SharedLearnedPeers::new(16));
        let domain_addr = SipAddr {
            addr: HostWithPort::try_from("sip.example.com:5060").unwrap(),
            r#type: Some(Transport::Udp),
        };
        learner.learn_from(&invite_request("c1"), Some(&domain_addr));
        assert!(learner.shared().is_empty());
        // Missing source address is ignored too.
        learner.learn_from(&invite_request("c2"), None);
        assert!(learner.shared().is_empty());
    }
}
