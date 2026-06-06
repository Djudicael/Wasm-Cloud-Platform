use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;
use common::auth::TrustedProxyNet;

fn extract_forwarded_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    if let Some(xff) = headers.get("X-Forwarded-For") {
        if let Ok(val) = xff.to_str() {
            if let Some(first) = val.split(',').next() {
                if let Ok(ip) = first.trim().parse() {
                    return Some(ip);
                }
            }
        }
    }

    if let Some(xri) = headers.get("X-Real-IP") {
        if let Ok(val) = xri.to_str() {
            if let Ok(ip) = val.parse() {
                return Some(ip);
            }
        }
    }

    None
}

fn peer_is_trusted(peer_ip: IpAddr, trusted_proxies: &[TrustedProxyNet]) -> bool {
    trusted_proxies.iter().any(|net| net.contains(&peer_ip))
}

/// Extract the client IP for admin API auth/rate limiting.
///
/// Forwarded headers are only honored when the immediate peer socket address
/// is in `trusted_proxies`. Otherwise the direct peer IP is used and spoofed
/// `X-Forwarded-For` / `X-Real-IP` headers are ignored.
pub fn extract_client_ip(
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
    trusted_proxies: &[TrustedProxyNet],
) -> Option<IpAddr> {
    let peer_ip = peer_addr.map(|addr| addr.ip());

    match peer_ip {
        Some(ip) if peer_is_trusted(ip, trusted_proxies) => {
            extract_forwarded_client_ip(headers).or(Some(ip))
        }
        Some(ip) => {
            if headers.contains_key("X-Forwarded-For") || headers.contains_key("X-Real-IP") {
                tracing::debug!(peer = %ip, "ignoring forwarded client IP headers from untrusted admin peer");
            }
            Some(ip)
        }
        None => None,
    }
}
