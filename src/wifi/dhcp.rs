//! Minimal RFC 2131 DHCP Responder for SoftAP Captive Portal.
//!
//! Serves DHCPDISCOVER and DHCPREQUEST on UDP port 67, automatically assigning
//! IP address 192.168.4.2 with Gateway 192.168.4.1 and DNS 192.168.4.1
//! so that connecting mobile devices (iOS / Android) and laptops obtain network
//! settings instantly and trigger OS Captive Network Assistant without external tools.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, info, warn};

pub const DHCP_SERVER_PORT: u16 = 67;
pub const DHCP_CLIENT_PORT: u16 = 68;
pub const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99]; // 0x63, 0x82, 0x53, 0x63

pub const DHCP_DISCOVER: u8 = 1;
pub const DHCP_OFFER: u8 = 2;
pub const DHCP_REQUEST: u8 = 3;
pub const DHCP_ACK: u8 = 5;

#[derive(Debug, PartialEq, Eq)]
pub struct DhcpPacket<'a> {
    pub op: u8,
    pub xid: [u8; 4],
    pub flags: u16,
    pub chaddr: &'a [u8],
    pub msg_type: u8,
}

/// Parses an incoming BOOTREQUEST DHCP packet.
/// Rejects packets shorter than 240 bytes or with invalid magic cookies.
pub fn parse_dhcp_request(buf: &[u8]) -> Option<DhcpPacket<'_>> {
    if buf.len() < 240 {
        return None;
    }

    let op = buf[0];
    if op != 1 {
        // Must be BOOTREQUEST (1)
        return None;
    }

    if buf[236..240] != MAGIC_COOKIE {
        return None;
    }

    let xid = [buf[4], buf[5], buf[6], buf[7]];
    let flags = u16::from_be_bytes([buf[10], buf[11]]);
    let chaddr = &buf[28..44];

    let mut msg_type = 0;
    let mut idx = 240;

    while idx < buf.len() {
        let tag = buf[idx];
        if tag == 0 {
            // PAD option
            idx += 1;
            continue;
        }
        if tag == 255 {
            // END option
            break;
        }

        if idx + 1 >= buf.len() {
            break;
        }
        let len = buf[idx + 1] as usize;
        let val_start = idx + 2;
        let val_end = val_start + len;

        if val_end > buf.len() {
            break; // Truncated option
        }

        if tag == 53 && len == 1 {
            msg_type = buf[val_start];
        }

        idx = val_end;
    }

    Some(DhcpPacket {
        op,
        xid,
        flags,
        chaddr,
        msg_type,
    })
}

/// Builds an RFC 2131 BOOTREPLY (DHCPOFFER or DHCPACK).
pub fn build_dhcp_reply(
    req: &DhcpPacket<'_>,
    assigned_ip: [u8; 4],
    server_ip: [u8; 4],
    is_ack: bool,
) -> Vec<u8> {
    let mut reply = vec![0u8; 240];

    reply[0] = 2; // BOOTREPLY
    reply[1] = 1; // Ethernet 10Mb
    reply[2] = 6; // MAC address length
    reply[3] = 0; // Hops

    reply[4..8].copy_from_slice(&req.xid);
    reply[8..10].copy_from_slice(&[0, 0]); // Secs
    reply[10..12].copy_from_slice(&req.flags.to_be_bytes());
    reply[12..16].copy_from_slice(&[0, 0, 0, 0]); // ciaddr
    reply[16..20].copy_from_slice(&assigned_ip); // yiaddr
    reply[20..24].copy_from_slice(&server_ip); // siaddr
    reply[24..28].copy_from_slice(&[0, 0, 0, 0]); // giaddr

    // Client hardware address
    let copy_len = req.chaddr.len().min(16);
    reply[28..28 + copy_len].copy_from_slice(&req.chaddr[..copy_len]);

    // Magic cookie
    reply[236..240].copy_from_slice(&MAGIC_COOKIE);

    // Options:
    // 53: Message Type (2 = Offer, 5 = ACK)
    reply.extend_from_slice(&[53, 1, if is_ack { DHCP_ACK } else { DHCP_OFFER }]);
    // 54: Server Identifier
    reply.extend_from_slice(&[
        54,
        4,
        server_ip[0],
        server_ip[1],
        server_ip[2],
        server_ip[3],
    ]);
    // 51: Lease Time (3600 seconds = 0x00000E10)
    reply.extend_from_slice(&[51, 4, 0, 0, 14, 16]);
    // 1: Subnet Mask (255.255.255.0)
    reply.extend_from_slice(&[1, 4, 255, 255, 255, 0]);
    // 3: Router / Default Gateway
    reply.extend_from_slice(&[3, 4, server_ip[0], server_ip[1], server_ip[2], server_ip[3]]);
    // 6: Domain Name Server
    reply.extend_from_slice(&[6, 4, server_ip[0], server_ip[1], server_ip[2], server_ip[3]]);
    // 255: End
    reply.push(255);

    reply
}

/// Runs the DHCP responder service on UDP socket.
pub async fn run_dhcp_server(
    bind_addr: SocketAddr,
    server_ip: [u8; 4],
    assigned_ip: [u8; 4],
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let socket = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("Failed to bind DHCP socket on {bind_addr}"))?;
    run_dhcp_server_on_socket(socket, server_ip, assigned_ip, shutdown_rx).await
}

/// Runs the DHCP responder service on an already bound UDP socket.
pub async fn run_dhcp_server_on_socket(
    socket: UdpSocket,
    server_ip: [u8; 4],
    assigned_ip: [u8; 4],
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let bind_addr = socket
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], DHCP_SERVER_PORT)));
    let _ = socket.set_broadcast(true);

    info!(
        addr = %bind_addr,
        server = format!("{}.{}.{}.{}", server_ip[0], server_ip[1], server_ip[2], server_ip[3]),
        lease = format!("{}.{}.{}.{}", assigned_ip[0], assigned_ip[1], assigned_ip[2], assigned_ip[3]),
        "DHCP captive server listening"
    );

    let mut buf = [0u8; 1024];

    loop {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    debug!("DHCP server received shutdown signal");
                    break;
                }
            }
            recv_res = socket.recv_from(&mut buf) => {
                match recv_res {
                    Ok((n, peer)) => {
                        if let Some(req) = parse_dhcp_request(&buf[..n]) {
                            match req.msg_type {
                                DHCP_DISCOVER => {
                                    debug!(%peer, "DHCPDISCOVER received, sending DHCPOFFER");
                                    let reply = build_dhcp_reply(&req, assigned_ip, server_ip, false);
                                    let dest = SocketAddr::from(([255, 255, 255, 255], DHCP_CLIENT_PORT));
                                    let subnet_dest = SocketAddr::from(([server_ip[0], server_ip[1], server_ip[2], 255], DHCP_CLIENT_PORT));
                                    if let Err(e) = socket.send_to(&reply, dest).await {
                                        if let Err(e2) = socket.send_to(&reply, subnet_dest).await {
                                            warn!(error = %e, fallback_error = %e2, "Failed to send DHCPOFFER");
                                        }
                                    }
                                }
                                DHCP_REQUEST => {
                                    info!(%peer, "DHCPREQUEST received, sending DHCPACK");
                                    let reply = build_dhcp_reply(&req, assigned_ip, server_ip, true);
                                    let dest = SocketAddr::from(([255, 255, 255, 255], DHCP_CLIENT_PORT));
                                    let subnet_dest = SocketAddr::from(([server_ip[0], server_ip[1], server_ip[2], 255], DHCP_CLIENT_PORT));
                                    if let Err(e) = socket.send_to(&reply, dest).await {
                                        if let Err(e2) = socket.send_to(&reply, subnet_dest).await {
                                            warn!(error = %e, fallback_error = %e2, "Failed to send DHCPACK");
                                        }
                                    }
                                }
                                other => {
                                    debug!(msg_type = other, "Ignoring non-discover/request DHCP message");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "DHCP socket receive error");
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

    fn make_test_request(msg_type: u8) -> Vec<u8> {
        let mut pkt = vec![0u8; 240];
        pkt[0] = 1; // BOOTREQUEST
        pkt[1] = 1; // Ethernet
        pkt[2] = 6;
        pkt[4..8].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]); // XID
        pkt[10..12].copy_from_slice(&[0x80, 0x00]); // Broadcast flag
        pkt[28..34].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]); // MAC
        pkt[236..240].copy_from_slice(&MAGIC_COOKIE);

        // Option 53
        pkt.extend_from_slice(&[53, 1, msg_type]);
        // Option 255 End
        pkt.push(255);
        pkt
    }

    #[test]
    fn test_parse_dhcp_discover_and_request() {
        let disc = make_test_request(DHCP_DISCOVER);
        let parsed = parse_dhcp_request(&disc).expect("Valid DHCPDISCOVER");
        assert_eq!(parsed.op, 1);
        assert_eq!(parsed.xid, [0x12, 0x34, 0x56, 0x78]);
        assert_eq!(parsed.flags, 0x8000);
        assert_eq!(parsed.msg_type, DHCP_DISCOVER);
        assert_eq!(&parsed.chaddr[..6], &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);

        let req = make_test_request(DHCP_REQUEST);
        let parsed_req = parse_dhcp_request(&req).expect("Valid DHCPREQUEST");
        assert_eq!(parsed_req.msg_type, DHCP_REQUEST);
    }

    #[test]
    fn test_parse_truncated_or_bad_magic_cookie_rejected() {
        // Less than 240 bytes
        let short = vec![0u8; 100];
        assert_eq!(parse_dhcp_request(&short), None);

        // Bad magic cookie
        let mut bad_cookie = make_test_request(DHCP_DISCOVER);
        bad_cookie[236..240].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(parse_dhcp_request(&bad_cookie), None);

        // Not BOOTREQUEST (op != 1)
        let mut bad_op = make_test_request(DHCP_DISCOVER);
        bad_op[0] = 2;
        assert_eq!(parse_dhcp_request(&bad_op), None);
    }

    #[test]
    fn test_build_dhcp_reply_offer_and_ack() {
        let disc = make_test_request(DHCP_DISCOVER);
        let parsed = parse_dhcp_request(&disc).unwrap();

        let server_ip = [192, 168, 4, 1];
        let assigned_ip = [192, 168, 4, 2];

        // 1. Build Offer
        let offer = build_dhcp_reply(&parsed, assigned_ip, server_ip, false);
        assert_eq!(offer[0], 2); // BOOTREPLY
        assert_eq!(&offer[4..8], &[0x12, 0x34, 0x56, 0x78]); // Echoed XID
        assert_eq!(&offer[16..20], &[192, 168, 4, 2]); // yiaddr
        assert_eq!(&offer[20..24], &[192, 168, 4, 1]); // siaddr
        assert_eq!(&offer[236..240], &MAGIC_COOKIE);

        // Verify options
        assert!(offer.windows(3).any(|w| w == [53, 1, DHCP_OFFER]));
        assert!(offer.windows(6).any(|w| w == [54, 4, 192, 168, 4, 1]));
        assert!(offer.windows(6).any(|w| w == [1, 4, 255, 255, 255, 0]));
        assert!(offer.windows(6).any(|w| w == [3, 4, 192, 168, 4, 1]));
        assert!(offer.windows(6).any(|w| w == [6, 4, 192, 168, 4, 1]));
        assert_eq!(offer.last(), Some(&255));

        // 2. Build ACK
        let ack = build_dhcp_reply(&parsed, assigned_ip, server_ip, true);
        assert!(ack.windows(3).any(|w| w == [53, 1, DHCP_ACK]));
    }

    #[tokio::test]
    async fn test_dhcp_server_loopback_exchange() {
        let server_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_socket.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let server_ip = [192, 168, 4, 1];
        let assigned_ip = [192, 168, 4, 2];

        let handle = tokio::spawn(async move {
            run_dhcp_server_on_socket(server_socket, server_ip, assigned_ip, shutdown_rx).await
        });

        // Client socket
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let disc = make_test_request(DHCP_DISCOVER);

        client.send_to(&disc, server_addr).await.unwrap();

        // Server processes discover
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let _ = shutdown_tx.send(true);
        let res = handle.await.unwrap();
        assert!(res.is_ok());
    }
}
