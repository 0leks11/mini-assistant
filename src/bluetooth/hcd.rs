//! Broadcom .hcd (Hex Command Data) firmware patch parser and HCI command generator.
//!
//! Broadcom Bluetooth chips (e.g. BCM43430 on Station Mini) require a patchram download
//! over UART upon boot. The .hcd file is structured as a series of HCI command packets
//! without the leading 0x01 packet type indicator:
//!   - Opcode: 2 bytes, little-endian
//!   - Length: 1 byte
//!   - Parameters: `Length` bytes

use anyhow::{bail, ensure, Result};

pub const HCI_COMMAND_PKT: u8 = 0x01;
pub const HCI_EVENT_PKT: u8 = 0x04;

pub const HCI_OGF_VENDOR: u16 = 0x3F;
pub const BCM_CMD_DOWNLOAD_MINIDRIVER: u16 = 0xFC2E;
pub const BCM_CMD_SET_BAUD_RATE: u16 = 0xFC18;
pub const HCI_CMD_RESET: u16 = 0x0C03;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HcdRecord {
    pub opcode: u16,
    pub params: Vec<u8>,
}

impl HcdRecord {
    /// Formats the record as an HCI Command Packet (starting with 0x01).
    pub fn to_hci_packet(&self) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(4 + self.params.len());
        pkt.push(HCI_COMMAND_PKT);
        let [lo, hi] = self.opcode.to_le_bytes();
        pkt.push(lo);
        pkt.push(hi);
        pkt.push(self.params.len() as u8);
        pkt.extend_from_slice(&self.params);
        pkt
    }
}

/// Parses an entire .hcd file buffer into a list of HCI command records.
pub fn parse_hcd(bytes: &[u8]) -> Result<Vec<HcdRecord>> {
    let mut records = Vec::new();
    let mut offset = 0;

    while offset < bytes.len() {
        ensure!(
            offset + 3 <= bytes.len(),
            "Truncated HCD header at offset {offset}: expected 3 bytes, got {}",
            bytes.len() - offset
        );

        let opcode = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let len = bytes[offset + 2] as usize;
        offset += 3;

        ensure!(
            offset + len <= bytes.len(),
            "Truncated HCD parameter payload at offset {offset}: expected {len} bytes, got {}",
            bytes.len() - offset
        );

        let params = bytes[offset..offset + len].to_vec();
        offset += len;

        records.push(HcdRecord { opcode, params });
    }

    Ok(records)
}

/// Formats the standard HCI Reset command packet (0x0C03).
pub fn format_hci_reset() -> [u8; 4] {
    [HCI_COMMAND_PKT, 0x03, 0x0C, 0x00]
}

/// Formats the Broadcom Download Minidriver command packet (0xFC2E).
pub fn format_bcm_minidriver() -> [u8; 4] {
    [HCI_COMMAND_PKT, 0x2E, 0xFC, 0x00]
}

/// Formats the Broadcom Set Baud Rate vendor command (0xFC18).
///
/// Parameters (6 bytes):
/// - 2 bytes: unused/reserved (0x00, 0x00)
/// - 4 bytes: 32-bit little-endian baud rate (e.g. 2,000,000 baud = 0x001E8480 -> 80 84 1E 00)
pub fn format_bcm_set_speed(baud: u32) -> [u8; 10] {
    let [b0, b1, b2, b3] = baud.to_le_bytes();
    [
        HCI_COMMAND_PKT,
        0x18,
        0xFC,
        0x06, // length: 6 bytes
        0x00,
        0x00, // reserved
        b0,
        b1,
        b2,
        b3,
    ]
}

/// Verifies that an incoming buffer contains a successful HCI Command Complete event (0x0E).
pub fn parse_command_complete(buf: &[u8], expected_opcode: u16) -> Result<()> {
    ensure!(
        buf.len() >= 6,
        "HCI event packet too short: {} bytes",
        buf.len()
    );
    ensure!(
        buf[0] == HCI_EVENT_PKT,
        "Expected HCI Event packet (0x04), got {:#04X}",
        buf[0]
    );
    ensure!(
        buf[1] == 0x0E,
        "Expected Command Complete event (0x0E), got {:#04X}",
        buf[1]
    );

    let len = buf[2] as usize;
    ensure!(
        buf.len() >= 3 + len,
        "Truncated HCI Event: header indicates {len} bytes, total packet is {}",
        buf.len()
    );

    let opcode = u16::from_le_bytes([buf[4], buf[5]]);
    ensure!(
        opcode == expected_opcode,
        "Opcode mismatch in Command Complete: expected {expected_opcode:#06X}, got {opcode:#06X}"
    );

    let status = buf[6];
    if status != 0x00 {
        bail!("HCI command {opcode:#06X} failed with status {status:#04X}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_hcd_records() {
        let mut data = Vec::new();

        // Record 1: opcode 0xFC4C, len 2, params [0x01, 0x02]
        data.extend_from_slice(&[0x4C, 0xFC, 0x02, 0x01, 0x02]);

        // Record 2: opcode 0xFC4D, len 0, params []
        data.extend_from_slice(&[0x4D, 0xFC, 0x00]);

        // Record 3: opcode 0xFC18, len 4, params [0x80, 0x84, 0x1E, 0x00]
        data.extend_from_slice(&[0x18, 0xFC, 0x04, 0x80, 0x84, 0x1E, 0x00]);

        let records = parse_hcd(&data).expect("HCD must parse successfully");
        assert_eq!(records.len(), 3);

        assert_eq!(records[0].opcode, 0xFC4C);
        assert_eq!(records[0].params, vec![0x01, 0x02]);

        assert_eq!(records[1].opcode, 0xFC4D);
        assert_eq!(records[1].params, Vec::<u8>::new());

        assert_eq!(records[2].opcode, 0xFC18);
        assert_eq!(records[2].params, vec![0x80, 0x84, 0x1E, 0x00]);

        // Verify to_hci_packet
        let pkt0 = records[0].to_hci_packet();
        assert_eq!(pkt0, vec![0x01, 0x4C, 0xFC, 0x02, 0x01, 0x02]);
    }

    #[test]
    fn test_parse_truncated_hcd_header_fails() {
        let truncated = [0x4C, 0xFC]; // Only 2 bytes, needs 3
        let err = parse_hcd(&truncated).unwrap_err();
        assert!(err.to_string().contains("Truncated HCD header"));
    }

    #[test]
    fn test_parse_truncated_hcd_payload_fails() {
        let truncated = [0x4C, 0xFC, 0x05, 0x01, 0x02]; // Length is 5, but only 2 bytes provided
        let err = parse_hcd(&truncated).unwrap_err();
        assert!(err.to_string().contains("Truncated HCD parameter payload"));
    }

    #[test]
    fn test_format_bcm_speed_command() {
        // 2,000,000 baud = 0x001E8480
        let cmd = format_bcm_set_speed(2_000_000);
        assert_eq!(
            cmd,
            [0x01, 0x18, 0xFC, 0x06, 0x00, 0x00, 0x80, 0x84, 0x1E, 0x00]
        );

        // 115,200 baud = 0x0001C200
        let cmd_115k = format_bcm_set_speed(115_200);
        assert_eq!(
            cmd_115k,
            [0x01, 0x18, 0xFC, 0x06, 0x00, 0x00, 0x00, 0xC2, 0x01, 0x00]
        );
    }

    #[test]
    fn test_parse_command_complete_success() {
        // [0x04 (event), 0x0E (cmd complete), 0x04 (len), 0x01 (num_pkts), 0x03, 0x0C (opcode 0x0C03), 0x00 (status ok)]
        let event = [0x04, 0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00];
        let res = parse_command_complete(&event, 0x0C03);
        assert!(res.is_ok());
    }

    #[test]
    fn test_parse_command_complete_error_status_fails() {
        // status = 0x01 (unknown HCI command)
        let event = [0x04, 0x0E, 0x04, 0x01, 0x03, 0x0C, 0x01];
        let err = parse_command_complete(&event, 0x0C03).unwrap_err();
        assert!(err.to_string().contains("failed with status 0x01"));
    }

    #[test]
    fn test_parse_command_complete_opcode_mismatch_fails() {
        // event for opcode 0xFC18, but we expected 0x0C03
        let event = [0x04, 0x0E, 0x04, 0x01, 0x18, 0xFC, 0x00];
        let err = parse_command_complete(&event, 0x0C03).unwrap_err();
        assert!(err.to_string().contains("Opcode mismatch"));
    }
}
