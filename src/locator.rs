use crate::config::RewriteRule;
use async_trait::async_trait;
use regex::Regex;
use rsipstack::Result;
use rsipstack::transaction::endpoint::TargetLocator;
use rsipstack::transport::SipAddr;

pub struct RewriteTargetLocator {
    rules: Vec<(Regex, String)>,
}

impl RewriteTargetLocator {
    pub fn new(rules: Vec<RewriteRule>) -> Self {
        let rules = rules
            .into_iter()
            .filter_map(|rule| match Regex::new(&rule.r#match) {
                Ok(re) => Some((re, rule.rewrite)),
                Err(e) => {
                    tracing::error!("Invalid rewrite rule pattern '{}': {}", rule.r#match, e);
                    None
                }
            })
            .collect();
        Self { rules }
    }
}

#[async_trait]
impl TargetLocator for RewriteTargetLocator {
    async fn locate(&self, uri: &rsip::Uri) -> Result<SipAddr> {
        let mut target_uri_str = uri.to_string();
        let mut matched = false;

        for (re, replacement) in &self.rules {
            if re.is_match(&target_uri_str) {
                let new_uri = re
                    .replace_all(&target_uri_str, replacement.as_str())
                    .to_string();
                tracing::debug!("Rewrite URI: {} -> {}", target_uri_str, new_uri);
                target_uri_str = new_uri;
                matched = true;
            }
        }

        if matched {
            let target_uri = rsip::Uri::try_from(target_uri_str.as_str())
                .map_err(|e| rsipstack::Error::Error(format!("Invalid rewritten URI: {}", e)))?;

            SipAddr::try_from(&target_uri)
        } else {
            SipAddr::try_from(uri)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsip::Uri;

    #[tokio::test]
    async fn test_rewrite_ip() {
        let rules = vec![RewriteRule {
            r#match: "116.116.116.116".to_string(),
            rewrite: "172.25.25.2".to_string(),
        }];
        let locator = RewriteTargetLocator::new(rules);

        let uri = Uri::try_from("sip:1001@116.116.116.116:5060").unwrap();
        let addr = locator.locate(&uri).await.unwrap();

        assert_eq!(addr.addr.to_string(), "172.25.25.2:5060");
    }

    #[tokio::test]
    async fn test_rewrite_regex() {
        let rules = vec![RewriteRule {
            r#match: "sip:(\\d+)@.*".to_string(),
            rewrite: "sip:$1@internal.net".to_string(),
        }];
        let locator = RewriteTargetLocator::new(rules);

        let uri = Uri::try_from("sip:12345@external.com").unwrap();
        let addr = locator.locate(&uri).await.unwrap();

        assert_eq!(addr.addr.to_string(), "internal.net");
    }

    #[tokio::test]
    async fn test_no_match() {
        let rules = vec![RewriteRule {
            r#match: "nomatch".to_string(),
            rewrite: "whatever".to_string(),
        }];
        let locator = RewriteTargetLocator::new(rules);

        let uri = Uri::try_from("sip:1001@116.62.75.161:5060").unwrap();
        let addr = locator.locate(&uri).await.unwrap();

        assert_eq!(addr.addr.to_string(), "116.62.75.161:5060");
    }

    #[tokio::test]
    async fn test_multiple_rules() {
        let rules = vec![
            RewriteRule {
                r#match: "116.62.75.161".to_string(),
                rewrite: "172.25.225.2".to_string(),
            },
            RewriteRule {
                r#match: "172.25.225.2".to_string(),
                rewrite: "10.0.0.1".to_string(),
            },
        ];
        let locator = RewriteTargetLocator::new(rules);

        let uri = Uri::try_from("sip:1001@116.62.75.161:5060").unwrap();
        let addr = locator.locate(&uri).await.unwrap();

        assert_eq!(addr.addr.to_string(), "10.0.0.1:5060");
    }
}
