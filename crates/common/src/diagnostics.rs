//! Public diagnostic projections must never echo connection credentials.
const REDACTED: &str = "[redacted]";
fn project_url(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 16384 || value.chars().any(char::is_control) {
        return None;
    }
    let mut url = url::Url::parse(value).ok()?;
    url.host_str()?;
    url.set_password(None).ok()?;
    url.set_username("").ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}
pub fn connection_url(value: &str) -> String {
    project_url(value).unwrap_or_else(|| REDACTED.into())
}
pub fn connection_urls(value: &str) -> String {
    if value.len() > 16384 {
        return REDACTED.into();
    }
    let mut urls = Vec::new();
    for entry in value.split(',') {
        if urls.len() == 16 {
            return REDACTED.into();
        }
        let Some(url) = project_url(entry.trim()) else {
            return REDACTED.into();
        };
        urls.push(url);
    }
    urls.join(",")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_diagnostics_remove_userinfo_query_and_fragments() {
        assert_eq!(
            connection_url(
                "postgresql://user:secret@localhost:5433/db?password=querysecret#fragmentsecret"
            ),
            "postgresql://localhost:5433/db"
        );
        assert_eq!(connection_urls("nats://user:secret@node-a:4222?token=query,tls://token-secret@[::1]:4223/path#fragment"),"nats://node-a:4222,tls://[::1]:4223/path");
        assert_eq!(
            connection_url(
                "https://u%40ser:encoded%40secret@example.test/path?secret=query#fragment"
            ),
            "https://example.test/path"
        );
    }
    #[test]
    fn invalid_diagnostics_never_fall_back_to_raw_input() {
        for value in [
            "not-a-url-secret",
            "postgresql://user:secret@",
            "nats://host\nsecret",
            "",
        ] {
            assert_eq!(connection_url(value), REDACTED);
        }
        assert_eq!(
            connection_urls("nats://host:4222,not-a-url-secret"),
            REDACTED
        );
        assert_eq!(
            connection_urls(&["nats://host:4222"; 17].join(",")),
            REDACTED
        );
        assert_eq!(connection_urls(&"secret".repeat(3000)), REDACTED);
    }
}
