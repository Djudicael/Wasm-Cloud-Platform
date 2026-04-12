// crates/proxy/src/config.rs
use std::time::Duration;

/// Timeouts configured at the Pingora HttpProxy level.
/// These apply to all connections before routing.
pub struct ProxyTimeouts {
    /// Maximum time to receive the full HTTP request headers.
    /// Defends against slowloris: if a client sends headers slower than this,
    /// the connection is dropped.
    pub request_header_read_timeout: Duration,

    /// Maximum time to receive the full HTTP request body.
    /// Prevents clients from holding connections open with slow uploads.
    pub request_body_read_timeout: Duration,

    /// Maximum time a keep-alive connection can stand idle between requests.
    pub keepalive_idle_timeout: Duration,

    /// Maximum size of HTTP request headers (bytes).
    /// Prevents memory exhaustion from oversized header attacks.
    pub max_header_size: usize,

    /// Maximum number of concurrent connections per source IP.
    /// Prevents a single source from monopolizing connection slots.
    pub max_connections_per_ip: u32,
}

impl Default for ProxyTimeouts {
    fn default() -> Self {
        ProxyTimeouts {
            request_header_read_timeout: Duration::from_secs(10),
            request_body_read_timeout: Duration::from_secs(30),
            keepalive_idle_timeout: Duration::from_secs(60),
            max_header_size: 8 * 1024, // 8 KB
            max_connections_per_ip: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_timeouts() {
        let timeouts = ProxyTimeouts::default();
        assert_eq!(
            timeouts.request_header_read_timeout,
            Duration::from_secs(10)
        );
        assert_eq!(timeouts.request_body_read_timeout, Duration::from_secs(30));
        assert_eq!(timeouts.keepalive_idle_timeout, Duration::from_secs(60));
        assert_eq!(timeouts.max_header_size, 8192);
        assert_eq!(timeouts.max_connections_per_ip, 256);
    }
}
