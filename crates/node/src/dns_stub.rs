//! Embedded DNS stub for resolving `*.internal` hostnames.
//!
//! Runs as a background UDP server inside the node process.
//! Answers `A` queries for any `*.internal` domain with `127.0.0.1`.
//! All other queries receive `SERVFAIL`, allowing the host resolver to try its
//! next configured nameserver for public/operator-managed DNS.
//!
//! This removes the need for external DNS (CoreDNS) or `/etc/hosts`
//! manipulation in test environments.

use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tracing::{info, warn};

/// Start the embedded DNS stub on the given bind address.
///
/// Returns the actual bound address (useful when bind is `127.0.0.1:0`).
pub async fn start_dns_stub(bind: SocketAddr) -> Result<SocketAddr, std::io::Error> {
    let socket = UdpSocket::bind(bind).await?;
    let addr = socket.local_addr()?;
    info!(%addr, "DNS stub listening for *.internal queries");

    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let response = build_response(&buf[..len]);
                    if let Err(e) = socket.send_to(&response, src).await {
                        warn!(error = %e, "DNS stub send failed");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "DNS stub recv failed");
                }
            }
        }
    });

    Ok(addr)
}

/// Minimal DNS response builder.
///
/// Parses a standard DNS query, checks if the queried name ends with
/// `.internal`, and returns an `A` record pointing to `127.0.0.1`.
/// Otherwise returns `SERVFAIL` so this authoritative split-DNS stub does not
/// claim that unrelated public names do not exist.
fn build_response(query: &[u8]) -> Vec<u8> {
    if query.len() < 12 {
        return Vec::new();
    }

    let tx_id = [query[0], query[1]];
    let flags = u16::from_be_bytes([query[2], query[3]]);

    // Only handle standard queries (QR = 0)
    if flags & 0x8000 != 0 {
        return Vec::new();
    }

    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        return Vec::new();
    }

    // Parse the question name
    let (name, name_len) = match parse_name(query, 12) {
        Some(v) => v,
        None => return Vec::new(),
    };

    let qtype_pos = 12 + name_len;
    if query.len() < qtype_pos + 4 {
        return Vec::new();
    }
    let qtype = u16::from_be_bytes([query[qtype_pos], query[qtype_pos + 1]]);
    let _qclass = u16::from_be_bytes([query[qtype_pos + 2], query[qtype_pos + 3]]);

    let is_internal = name.ends_with(".internal");
    let is_a_query = qtype == 1; // A record

    // Build response
    let mut resp = Vec::with_capacity(512);

    // Header
    resp.extend_from_slice(&tx_id); // Transaction ID
    if is_internal && is_a_query {
        resp.extend_from_slice(&[0x81, 0x80]); // QR=1, AA=1, RCODE=0 (NOERROR)
    } else {
        resp.extend_from_slice(&[0x81, 0x82]); // QR=1, AA=1, RCODE=2 (SERVFAIL)
    }
    resp.extend_from_slice(&[0x00, 0x01]); // QDCOUNT = 1
    if is_internal && is_a_query {
        resp.extend_from_slice(&[0x00, 0x01]); // ANCOUNT = 1
    } else {
        resp.extend_from_slice(&[0x00, 0x00]); // ANCOUNT = 0
    }
    resp.extend_from_slice(&[0x00, 0x00]); // NSCOUNT = 0
    resp.extend_from_slice(&[0x00, 0x00]); // ARCOUNT = 0

    // Question section (copy from query)
    resp.extend_from_slice(&query[12..qtype_pos + 4]);

    // Answer section (only for internal A queries)
    if is_internal && is_a_query {
        // Name (pointer to question name at offset 12)
        resp.push(0xC0);
        resp.push(0x0C);
        // Type A
        resp.extend_from_slice(&[0x00, 0x01]);
        // Class IN
        resp.extend_from_slice(&[0x00, 0x01]);
        // TTL
        resp.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]); // 60 seconds
                                                           // RDLENGTH
        resp.extend_from_slice(&[0x00, 0x04]);
        // RDATA: 127.0.0.1
        resp.extend_from_slice(&[127, 0, 0, 1]);
    }

    resp
}

/// Parse a DNS name from a query packet.
/// Returns (name_string, bytes_consumed).
fn parse_name(packet: &[u8], offset: usize) -> Option<(String, usize)> {
    let mut pos = offset;
    let mut name = String::new();
    let mut jumped = false;
    let mut len_consumed = 0;

    loop {
        if pos >= packet.len() {
            return None;
        }
        let len = packet[pos] as usize;

        if len == 0 {
            pos += 1;
            if !jumped {
                len_consumed = pos - offset;
            }
            break;
        }

        // Compression pointer
        if len & 0xC0 == 0xC0 {
            if pos + 1 >= packet.len() {
                return None;
            }
            let pointer = ((len & 0x3F) << 8) | (packet[pos + 1] as usize);
            if !jumped {
                jumped = true;
                len_consumed = (pos + 2) - offset;
            }
            pos = pointer;
            continue;
        }

        if len > 63 || pos + 1 + len > packet.len() {
            return None;
        }

        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(&packet[pos + 1..pos + 1 + len]).ok()?);
        pos += 1 + len;
    }

    Some((name, len_consumed))
}

#[cfg(test)]
mod tests {
    use super::build_response;

    fn a_query(name: &str) -> Vec<u8> {
        let mut query = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            query.push(label.len() as u8);
            query.extend_from_slice(label.as_bytes());
        }
        query.push(0);
        query.extend_from_slice(&[0, 1, 0, 1]);
        query
    }

    #[test]
    fn internal_a_query_resolves_to_loopback() {
        let response = build_response(&a_query("api.production.internal"));
        assert_eq!(&response[2..4], &[0x81, 0x80]);
        assert_eq!(&response[6..8], &[0, 1]);
        assert_eq!(&response[response.len() - 4..], &[127, 0, 0, 1]);
    }

    #[test]
    fn unrelated_name_returns_servfail_for_resolver_fallback() {
        let response = build_response(&a_query("example.com"));
        assert_eq!(&response[2..4], &[0x81, 0x82]);
        assert_eq!(&response[6..8], &[0, 0]);
    }
}
