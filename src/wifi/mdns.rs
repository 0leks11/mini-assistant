//! Zero-dependency mDNS (Multicast DNS) Responder for GPT Mini.
//!
//! Complies with RFC 6762 (Multicast DNS) to resolve `http://gpt-mini.local/`
//! directly on iOS (Safari/Bonjour), Android (Chrome mDNS), macOS, Linux, and Windows
//! without needing a local DNS server or hardcoded IP addresses.

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

pub const MDNS_IPV4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_PORT: u16 = 5353;
pub const DEFAULT_HOSTNAME: &str = "gpt-mini.local";
pub const DEFAULT_TTL_SECS: u32 = 120;

const TYPE_A: u16 = 1;
const _TYPE_PTR: u16 = 12;
const CLASS_IN: u16 = 1;
const CACHE_FLUSH_BIT: u16 = 0x8000;

/// Encodes a domain name (e.g. "gpt-mini.local") into standard DNS wire format labels.
pub fn encode_dns_name(name: &str) -> Vec<u8> {
    let clean = name.trim_matches('.');
    let mut out = Vec::with_capacity(clean.len() + 2);
    for part in clean.split('.') {
        if part.is_empty() {
            continue;
        }
        let len = part.len().min(63) as u8;
        out.push(len);
        out.extend_from_slice(&part.as_bytes()[..len as usize]);
    }
    out.push(0);
    out
}

/// Parses standard DNS wire labels into a dotted lowercase string.
pub fn parse_dns_name(buf: &[u8], mut offset: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut visited = 0;
    let original_offset = offset;
    let mut jumped = false;
    let mut end_offset = offset;

    while offset < buf.len() && visited < 128 {
        visited += 1;
        let len = buf[offset] as usize;
        if len == 0 {
            if !jumped {
                end_offset = offset + 1;
            }
            break;
        }

        // Pointer (0xC0)
        if len & 0xC0 == 0xC0 {
            if offset + 1 >= buf.len() {
                return None;
            }
            let ptr = (((len & 0x3F) as usize) << 8) | (buf[offset + 1] as usize);
            if ptr >= original_offset && !jumped {
                return None; // Prevent pointer loops forward/backward
            }
            if !jumped {
                end_offset = offset + 2;
                jumped = true;
            }
            offset = ptr;
            continue;
        }

        offset += 1;
        if offset + len > buf.len() {
            return None;
        }

        let label = std::str::from_utf8(&buf[offset..offset + len]).ok()?;
        labels.push(label.to_ascii_lowercase());
        offset += len;
        if !jumped {
            end_offset = offset;
        }
    }

    if labels.is_empty() {
        Some((".".to_string(), end_offset))
    } else {
        Some((labels.join("."), end_offset))
    }
}

/// Represents a parsed DNS question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsQuestion {
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    pub unicast_response: bool,
}

/// Parses questions from a DNS/mDNS packet.
pub fn parse_dns_questions(packet: &[u8]) -> Vec<DnsQuestion> {
    if packet.len() < 12 {
        return Vec::new();
    }

    let qdcount = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    if qdcount == 0 {
        return Vec::new();
    }

    let mut questions = Vec::with_capacity(qdcount);
    let mut cursor = 12;

    for _ in 0..qdcount {
        let (name, next_cursor) = match parse_dns_name(packet, cursor) {
            Some(res) => res,
            None => break,
        };
        cursor = next_cursor;

        if cursor + 4 > packet.len() {
            break;
        }

        let qtype = u16::from_be_bytes([packet[cursor], packet[cursor + 1]]);
        let raw_class = u16::from_be_bytes([packet[cursor + 2], packet[cursor + 3]]);
        cursor += 4;

        let unicast_response = (raw_class & 0x8000) != 0;
        let qclass = raw_class & 0x7FFF;

        questions.push(DnsQuestion {
            name,
            qtype,
            qclass,
            unicast_response,
        });
    }

    questions
}

/// Builds an authoritative mDNS A record response packet for `name -> ip`.
pub fn build_mdns_a_response(name: &str, ip: [u8; 4], ttl_secs: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(64);

    // Header:
    // ID: 0 (RFC 6762 mDNS standard)
    packet.extend_from_slice(&[0x00, 0x00]);
    // Flags: QR=1 (response), AA=1 (authoritative) -> 0x8400
    packet.extend_from_slice(&[0x84, 0x00]);
    // QDCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00]);
    // ANCOUNT: 1
    packet.extend_from_slice(&[0x00, 0x01]);
    // NSCOUNT: 0, ARCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // Answer Record:
    // Name
    packet.extend_from_slice(&encode_dns_name(name));
    // TYPE: 1 (A)
    packet.extend_from_slice(&TYPE_A.to_be_bytes());
    // CLASS: IN + Cache-Flush bit (0x8001)
    let class = CLASS_IN | CACHE_FLUSH_BIT;
    packet.extend_from_slice(&class.to_be_bytes());
    // TTL
    packet.extend_from_slice(&ttl_secs.to_be_bytes());
    // RDLENGTH: 4
    packet.extend_from_slice(&4u16.to_be_bytes());
    // RDATA: IPv4 octets
    packet.extend_from_slice(&ip);

    packet
}

/// Discovers the current active IPv4 address assigned to the device.
pub fn discover_local_ipv4() -> Option<Ipv4Addr> {
    // 1. Preferred method: route test via UDP socket without sending packets
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("10.77.0.1:53").is_ok() || socket.connect("8.8.8.8:53").is_ok() {
            if let Ok(SocketAddr::V4(addr)) = socket.local_addr() {
                if !addr.ip().is_loopback() && !addr.ip().is_unspecified() {
                    return Some(*addr.ip());
                }
            }
        }
    }

    // 2. Fallback: Parse `ifconfig wlan0`
    if let Ok(out) = std::process::Command::new("ifconfig").arg("wlan0").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(pos) = text.find("inet addr:") {
            if let Some(ip_str) = text[pos + 10..].split_whitespace().next() {
                if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
                    return Some(ip);
                }
            }
        }
    }

    None
}

/// Creates a UDP socket joined to the mDNS multicast group `224.0.0.251:5353`
/// with `SO_REUSEADDR` and `SO_REUSEPORT` enabled so it co-exists with Spotify Connect / soloist.
pub fn create_mdns_socket() -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("Failed to create raw UDP socket")?;

    socket
        .set_reuse_address(true)
        .context("Failed to set SO_REUSEADDR")?;

    #[cfg(unix)]
    socket
        .set_reuse_port(true)
        .context("Failed to set SO_REUSEPORT")?;

    socket
        .set_nonblocking(true)
        .context("Failed to set non-blocking")?;

    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), MDNS_PORT);
    socket
        .bind(&bind_addr.into())
        .context("Failed to bind 0.0.0.0:5353")?;

    socket
        .join_multicast_v4(&MDNS_IPV4, &Ipv4Addr::UNSPECIFIED)
        .context("Failed to join mDNS multicast group 224.0.0.251")?;

    let std_socket: std::net::UdpSocket = socket.into();
    let async_socket = UdpSocket::from_std(std_socket).context("Failed to wrap into tokio UdpSocket")?;

    Ok(async_socket)
}

/// Spawns the background mDNS responder task for `gpt-mini.local`.
pub fn spawn_mdns_responder(
    target_hostname: String,
    http_port: u16,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            hostname = %target_hostname,
            http_port,
            "Starting zero-config mDNS responder (http://{}/)", target_hostname
        );

        let socket = match create_mdns_socket() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to bind mDNS socket; http://{}/ resolution will rely on external DNS", target_hostname);
                return;
            }
        };

        let target_name = target_hostname.to_ascii_lowercase();
        let target_name_dotted = format!("{}.", target_name);
        let multicast_dest: SocketAddr = SocketAddr::new(IpAddr::V4(MDNS_IPV4), MDNS_PORT);

        // Periodic gratuitous announcement timer (RFC 6762 §8.3)
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        let mut buf = [0u8; 1500];

        // Send initial announcements immediately to prime local network caches
        if let Some(ip) = discover_local_ipv4() {
            let resp = build_mdns_a_response(&target_name, ip.octets(), DEFAULT_TTL_SECS);
            let _ = socket.send_to(&resp, multicast_dest).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = socket.send_to(&resp, multicast_dest).await;
            info!(
                ip = %ip,
                "Broadcasted initial mDNS announcement: http://{}/ -> {}",
                target_name,
                ip
            );
        }

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Some(ip) = discover_local_ipv4() {
                        let resp = build_mdns_a_response(&target_name, ip.octets(), DEFAULT_TTL_SECS);
                        let _ = socket.send_to(&resp, multicast_dest).await;
                        debug!(ip = %ip, "Periodic mDNS announcement refreshed");
                    }
                }
                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, peer)) => {
                            let packet = &buf[..len];
                            let questions = parse_dns_questions(packet);

                            for q in questions {
                                let qname_lower = q.name.to_ascii_lowercase();
                                let matches_host = qname_lower == target_name
                                    || qname_lower == target_name_dotted
                                    || qname_lower == "gpt-mini"
                                    || qname_lower == "gpt-mini.";

                                if matches_host && (q.qtype == TYPE_A || q.qtype == 255 /* ANY */) {
                                    if let Some(ip) = discover_local_ipv4() {
                                        let resp = build_mdns_a_response(&target_name, ip.octets(), DEFAULT_TTL_SECS);

                                        // Reply directly to querier (unicast)
                                        let _ = socket.send_to(&resp, peer).await;

                                        // Also multicast if requested
                                        if !q.unicast_response {
                                            let _ = socket.send_to(&resp, multicast_dest).await;
                                        }

                                        debug!(
                                            %peer,
                                            qname = %q.name,
                                            ip = %ip,
                                            "Answered mDNS query for http://{}/",
                                            target_name
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "mDNS recv error");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_dns_name() {
        let encoded = encode_dns_name("gpt-mini.local");
        assert_eq!(
            encoded,
            vec![
                8, b'g', b'p', b't', b'-', b'm', b'i', b'n', b'i', 5, b'l', b'o', b'c', b'a',
                b'l', 0
            ]
        );
    }

    #[test]
    fn test_parse_dns_name_roundtrip() {
        let encoded = encode_dns_name("gpt-mini.local");
        let (name, offset) = parse_dns_name(&encoded, 0).expect("parse valid name");
        assert_eq!(name, "gpt-mini.local");
        assert_eq!(offset, encoded.len());
    }

    #[test]
    fn test_build_mdns_a_response() {
        let packet = build_mdns_a_response("gpt-mini.local", [10, 77, 0, 111], 120);

        assert!(packet.len() > 12);
        // Flags: 0x8400
        assert_eq!(packet[2], 0x84);
        assert_eq!(packet[3], 0x00);
        // ANCOUNT = 1
        assert_eq!(packet[6], 0x00);
        assert_eq!(packet[7], 0x01);

        // Check that IP appears in RDATA (last 4 bytes)
        let last_4 = &packet[packet.len() - 4..];
        assert_eq!(last_4, &[10, 77, 0, 111]);
    }

    #[test]
    fn test_parse_dns_questions_query() {
        let mut query = vec![
            0x12, 0x34, // ID
            0x00, 0x00, // Flags (standard query)
            0x00, 0x01, // QDCOUNT: 1
            0x00, 0x00, // ANCOUNT: 0
            0x00, 0x00, // NSCOUNT: 0
            0x00, 0x00, // ARCOUNT: 0
        ];
        query.extend_from_slice(&encode_dns_name("gpt-mini.local"));
        query.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
        query.extend_from_slice(&0x8001u16.to_be_bytes()); // CLASS IN + QU bit

        let questions = parse_dns_questions(&query);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "gpt-mini.local");
        assert_eq!(questions[0].qtype, 1);
        assert_eq!(questions[0].qclass, 1);
        assert!(questions[0].unicast_response);
    }
}
