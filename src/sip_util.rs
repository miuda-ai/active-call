//! Small SIP helpers shared across modules.

/// Ensure a SIP URI carries a `sip:`/`sips:` scheme, prepending `sip:` when
/// missing.
pub fn ensure_sip_scheme(uri: String) -> String {
    if uri.starts_with("sip:") || uri.starts_with("sips:") {
        uri
    } else {
        format!("sip:{}", uri)
    }
}

/// Convert a map of header name/value pairs into SIP headers.
pub fn sip_headers_from_map(
    headers: &std::collections::HashMap<String, String>,
) -> Vec<rsipstack::rsip::Header> {
    headers
        .iter()
        .map(|(k, v)| rsipstack::rsip::Header::Other(k.clone(), v.clone()))
        .collect()
}

/// Extract the hangup-headers map stored under `_hangup_headers` in extras,
/// converted to SIP headers.
pub fn hangup_headers_from_extras(
    extras: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<Vec<rsipstack::rsip::Header>> {
    let value = extras.get("_hangup_headers")?;
    let map =
        serde_json::from_value::<std::collections::HashMap<String, String>>(value.clone()).ok()?;
    let headers = sip_headers_from_map(&map);
    (!headers.is_empty()).then_some(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_scheme_when_missing() {
        assert_eq!(
            ensure_sip_scheme("bob@example.com".into()),
            "sip:bob@example.com"
        );
        assert_eq!(
            ensure_sip_scheme("sip:bob@example.com".into()),
            "sip:bob@example.com"
        );
        assert_eq!(
            ensure_sip_scheme("sips:bob@example.com".into()),
            "sips:bob@example.com"
        );
    }

    #[test]
    fn converts_header_maps() {
        let mut map = std::collections::HashMap::new();
        map.insert("X-Job-Id".to_string(), "42".to_string());
        let headers = sip_headers_from_map(&map);
        assert_eq!(headers.len(), 1);
        assert!(matches!(&headers[0], rsipstack::rsip::Header::Other(k, v)
            if k == "X-Job-Id" && v == "42"));
    }

    #[test]
    fn extracts_hangup_headers_from_extras() {
        let mut extras = std::collections::HashMap::new();
        assert!(hangup_headers_from_extras(&extras).is_none());

        let mut map = std::collections::HashMap::new();
        map.insert("X-Reason".to_string(), "done".to_string());
        extras.insert(
            "_hangup_headers".to_string(),
            serde_json::to_value(&map).unwrap(),
        );
        assert!(hangup_headers_from_extras(&extras).is_some());
    }
}
