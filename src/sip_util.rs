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

/// Extract the dialable target URI from a `Refer-To` header value.
///
/// Accepts the common RFC 3515 spellings:
/// - `<sip:1002@host:port>`
/// - `sip:1002@host:port`
/// - `"Bob" <sip:1002@host:port>;foo=bar`
///
/// Display names, angle brackets, URI parameters (`;...`) and the query part
/// (`?Replaces=...`, attended transfer) are stripped; only the plain
/// `scheme:user@host[:port]` remains.
pub fn parse_refer_to_target(refer_to: &str) -> Option<String> {
    let value = refer_to.trim();
    if value.is_empty() {
        return None;
    }
    let inner = match (value.find('<'), value.rfind('>')) {
        (Some(start), Some(end)) if end > start => &value[start + 1..end],
        // A '>' without a matching '<' means the raw value contained one;
        // fall through to the unbracketed path which will strip params anyway.
        _ => value,
    };
    // Drop URI parameters and the query (e.g. ?Replaces=...).
    let inner = inner.split([';', '?']).next()?;
    let inner = inner.trim();
    (!inner.is_empty()).then(|| inner.to_string())
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
    fn parses_refer_to_targets() {
        assert_eq!(
            parse_refer_to_target("<sip:1002@127.0.0.1:5080>").as_deref(),
            Some("sip:1002@127.0.0.1:5080")
        );
        assert_eq!(
            parse_refer_to_target("sip:1002@127.0.0.1:5080").as_deref(),
            Some("sip:1002@127.0.0.1:5080")
        );
        assert_eq!(
            parse_refer_to_target("\"Bob\" <sip:1002@host>;foo=bar").as_deref(),
            Some("sip:1002@host")
        );
        // Attended-transfer query (Replaces) is stripped.
        assert_eq!(
            parse_refer_to_target(
                "<sip:1003@host?Replaces=abc%40host%3Bto-tag%3D1%3Bfrom-tag%3D2>"
            )
            .as_deref(),
            Some("sip:1003@host")
        );
        assert_eq!(parse_refer_to_target(""), None);
        assert_eq!(parse_refer_to_target("   "), None);
        assert_eq!(parse_refer_to_target("<>;foo=bar"), None);
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
