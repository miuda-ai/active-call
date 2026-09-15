#[cfg(test)]
mod tests {
    use rsipstack::rsip::Uri;

    #[test]
    fn test_uri_parsing() {
        // Test various URI formats
        let test_cases = vec![
            ("sip:1001@10.8.0.1", "10.8.0.1"),
            ("sip:user@example.com", "example.com"),
            ("sip:1001@10.8.0.1:5060", "10.8.0.1"),
            ("sip:user@example.com:5060", "example.com"),
            ("1001@10.8.0.1", "10.8.0.1"),
            ("user@example.com", "example.com"),
        ];

        for (input, expected_host) in test_cases {
            let uri_str = if !input.starts_with("sip:") && !input.starts_with("sips:") {
                format!("sip:{}", input)
            } else {
                input.to_string()
            };

            let parsed =
                Uri::try_from(uri_str.as_str()).expect(&format!("Failed to parse: {}", input));

            let host = match &parsed.host_with_port.host {
                rsipstack::rsip::Host::Domain(domain) => domain.to_string(),
                rsipstack::rsip::Host::IpAddr(ip) => ip.to_string(),
            };

            assert_eq!(host, expected_host, "Failed for input: {}", input);
            println!("✓ {} -> {}", input, host);
        }
    }

    #[test]
    fn test_ip_address_parsing() {
        let uri = Uri::try_from("sip:1001@192.168.1.1:5060").unwrap();
        match &uri.host_with_port.host {
            rsipstack::rsip::Host::IpAddr(ip) => {
                assert_eq!(ip.to_string(), "192.168.1.1");
                println!("✓ IP address parsed correctly: {}", ip);
            }
            _ => panic!("Expected IP address"),
        }
    }

    #[test]
    fn test_domain_parsing() {
        let uri = Uri::try_from("sip:user@example.com:5060").unwrap();
        match &uri.host_with_port.host {
            rsipstack::rsip::Host::Domain(domain) => {
                assert_eq!(domain.to_string(), "example.com");
                println!("✓ Domain parsed correctly: {}", domain);
            }
            _ => panic!("Expected domain"),
        }
    }
}
