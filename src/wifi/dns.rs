//! Lightweight Captive DNS Responder.
//!
//! Intercepts all DNS queries (like `captive.apple.com`, `connectivitycheck.gstatic.com`)
//! and redirects them to the local SoftAP web portal IP (e.g. `192.168.4.1`).

use anyhow::{Context, Result};
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, warn};

pub const DEFAULT_PORTAL_IP: [u8; 4] = [192, 168, 4, 1];
const TYPE_A: u16 = 1;

/// Parses a DNS query and constructs an authoritative response.
///
/// For IPv4 `A` record queries, it returns the target `ip`.
/// For other query types (like `AAAA` IPv6), it returns `NOERROR` with 0 answers,
/// prompting the client OS to immediately fall back to IPv4.
pub fn build_dns_response(query: &[u8], target_ip: [u8; 4]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }

    let id = [query[0], query[1]];
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount == 0 {
        return None;
    }

    // Parse question section
    let mut cursor = 12;
    while cursor < query.len() {
        let len = query[cursor] as usize;
        if len == 0 {
            cursor += 1;
            break;
        }
        if cursor + 1 + len > query.len() {
            return None; // Malformed label length
        }
        cursor += 1 + len;
    }

    if cursor + 4 > query.len() {
        return None; // Missing QTYPE and QCLASS
    }

    let qtype = u16::from_be_bytes([query[cursor], query[cursor + 1]]);
    let qclass = u16::from_be_bytes([query[cursor + 2], query[cursor + 3]]);
    cursor += 4;

    let question_len = cursor;
    let mut response = Vec::with_capacity(question_len + 16);

    // Header:
    // ID
    response.extend_from_slice(&id);
    // Flags: QR=1 (Response), Opcode=0, AA=1 (Authoritative), RD=echo, RA=1 (Recursion available), RCODE=0
    let flags: u16 = 0x8180;
    response.extend_from_slice(&flags.to_be_bytes());
    // QDCOUNT: 1
    response.extend_from_slice(&1u16.to_be_bytes());

    if qtype == TYPE_A && qclass == 1 {
        // ANCOUNT: 1
        response.extend_from_slice(&1u16.to_be_bytes());
        // NSCOUNT: 0, ARCOUNT: 0
        response.extend_from_slice(&0u32.to_be_bytes());

        // Echo question
        response.extend_from_slice(&query[12..question_len]);

        // Answer Section:
        // Pointer to QNAME at byte 12 (0xC00C)
        response.extend_from_slice(&[0xC0, 0x0C]);
        // Type: A (1)
        response.extend_from_slice(&TYPE_A.to_be_bytes());
        // Class: IN (1)
        response.extend_from_slice(&1u16.to_be_bytes());
        // TTL: 60 seconds
        response.extend_from_slice(&60u32.to_be_bytes());
        // RDLENGTH: 4
        response.extend_from_slice(&4u16.to_be_bytes());
        // RDATA: IPv4 bytes
        response.extend_from_slice(&target_ip);
    } else {
        // For non-A queries: return NOERROR with 0 answers
        response.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT = 0
        response.extend_from_slice(&0u32.to_be_bytes()); // NSCOUNT=0, ARCOUNT=0
        response.extend_from_slice(&query[12..question_len]);
    }

    Some(response)
}

/// Runs the captive DNS server until `shutdown_rx` signals true.
pub async fn run_dns_server(
    bind_addr: SocketAddr,
    target_ip: [u8; 4],
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let socket = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("Cannot bind DNS server to {bind_addr}"))?;

    let mut buf = [0u8; 512];

    loop {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    debug!("Captive DNS server received shutdown signal");
                    break;
                }
            }
            recv_res = socket.recv_from(&mut buf) => {
                match recv_res {
                    Ok((len, peer)) => {
                        if let Some(resp) = build_dns_response(&buf[..len], target_ip) {
                            if let Err(e) = socket.send_to(&resp, peer).await {
                                warn!(%peer, error = %e, "Failed to send DNS response");
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "DNS socket recv error");
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_query(domain: &str, qtype: u16) -> Vec<u8> {
        let mut packet = Vec::new();
        // ID: 0x1234
        packet.extend_from_slice(&[0x12, 0x34]);
        // Flags: Standard query (0x0100)
        packet.extend_from_slice(&[0x01, 0x00]);
        // QDCOUNT: 1
        packet.extend_from_slice(&[0x00, 0x01]);
        // ANCOUNT, NSCOUNT, ARCOUNT: 0
        packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        // QNAME
        for label in domain.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0); // Root label

        // QTYPE
        packet.extend_from_slice(&qtype.to_be_bytes());
        // QCLASS: IN (1)
        packet.extend_from_slice(&1u16.to_be_bytes());

        packet
    }

    #[test]
    fn test_dns_a_record_response() {
        let query = make_test_query("captive.apple.com", TYPE_A);
        let resp = build_dns_response(&query, [192, 168, 4, 1]).expect("Valid response");

        // Assert ID echoed
        assert_eq!(&resp[0..2], &[0x12, 0x34]);
        // Assert Response flags (QR=1, AA=1, RA=1, RCODE=0)
        assert_eq!(&resp[2..4], &[0x81, 0x80]);
        // Assert QDCOUNT=1, ANCOUNT=1
        assert_eq!(&resp[4..8], &[0x00, 0x01, 0x00, 0x01]);

        // Assert Answer RDATA is 192.168.4.1 (last 4 bytes)
        let rdata = &resp[resp.len() - 4..];
        assert_eq!(rdata, &[192, 168, 4, 1]);
    }

    #[test]
    fn test_dns_aaaa_record_returns_no_error_with_zero_answers() {
        let query = make_test_query("connectivitycheck.gstatic.com", 28); // 28 = AAAA
        let resp = build_dns_response(&query, [192, 168, 4, 1]).expect("Valid response");

        // Assert QDCOUNT=1, ANCOUNT=0
        assert_eq!(&resp[4..8], &[0x00, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn test_truncated_or_malformed_packets_rejected_without_panic() {
        assert!(build_dns_response(&[], [192, 168, 4, 1]).is_none());
        assert!(build_dns_response(&[0x12, 0x34], [192, 168, 4, 1]).is_none());

        // Malformed label len pointing past packet
        let mut malformed = make_test_query("example.com", TYPE_A);
        malformed[12] = 250; // invalid label length
        assert!(build_dns_response(&malformed, [192, 168, 4, 1]).is_none());
    }

    #[tokio::test]
    async fn test_dns_server_loopback_integration() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = UdpSocket::bind(bind_addr).await.unwrap();
        let actual_addr = socket.local_addr().unwrap();
        drop(socket);

        let server_handle = tokio::spawn(async move {
            run_dns_server(actual_addr, [192, 168, 4, 1], shutdown_rx).await
        });

        // Client test
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let query = make_test_query("hotspot-detect.html", TYPE_A);
        client.send_to(&query, actual_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buf),
        )
        .await
        .expect("DNS response timeout")
        .expect("Recv error");

        assert!(len > 12);
        assert_eq!(&buf[len - 4..len], &[192, 168, 4, 1]);

        let _ = shutdown_tx.send(true);
        let _ = server_handle.await;
    }
}
