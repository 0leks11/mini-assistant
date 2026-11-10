//! Bluetooth and Broadcom BCM43430 UART loader subsystem.

pub mod hcd;

#[allow(unused_imports)]
use anyhow::{Context, Result};
#[allow(unused_imports)]
use tracing::info;

/// Enable Bluetooth discoverable and connectable mode on `hci0`.
/// Sets the local device name to "GPT Mini", EIR with Audio Sink UUID, and enables inquiry + page scanning.
pub async fn enable_discoverable(dev_id: i32) -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        tokio::task::spawn_blocking(move || linux_bt::set_discoverable_linux(dev_id))
            .await
            .context("Bluetooth task panicked")?
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dev_id;
        info!("Bluetooth mock: enable_discoverable invoked for non-linux");
        Ok("Колонка GPT Mini доступна для подключения по Bluetooth (симуляция)".to_string())
    }
}

#[cfg(target_os = "linux")]
pub mod linux_bt {
    #[allow(unused_imports)]
    use anyhow::{Context, Result};
    use tracing::{info, warn};

    const AF_BLUETOOTH: libc::c_int = 31;
    const BTPROTO_HCI: libc::c_int = 1;
    const HCI_CHANNEL_RAW: u16 = 0;

    const HCIGETDEVINFO: libc::c_ulong = 0x800448D3;
    const HCISETSCAN: libc::c_ulong = 0x400448DD;

    const SCAN_PAGE: u32 = 0x01;
    const SCAN_INQUIRY: u32 = 0x02;

    #[repr(C)]
    struct SockaddrHci {
        hci_family: libc::sa_family_t,
        hci_dev: u16,
        hci_channel: u16,
    }

    #[repr(C)]
    struct HciDevReq {
        dev_id: u16,
        dev_opt: u32,
    }

    #[repr(C)]
    struct HciDevInfo {
        dev_id: u16,
        name: [u8; 8],
        bdaddr: [u8; 6],
        flags: u32,
        dev_type: u8,
        features: [u8; 8],
        pkt_type: u32,
        link_policy: u32,
        link_mode: u32,
        acl_mtu: u16,
        acl_pkts: u16,
        sco_mtu: u16,
        sco_pkts: u16,
        stat: [u8; 40],
    }

    pub fn set_discoverable_linux(dev_id: i32) -> Result<String> {
        unsafe {
            let sock = libc::socket(AF_BLUETOOTH, libc::SOCK_RAW, BTPROTO_HCI);
            if sock < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Failed to open AF_BLUETOOTH raw socket");
            }

            let mut sa: SockaddrHci = std::mem::zeroed();
            sa.hci_family = AF_BLUETOOTH as libc::sa_family_t;
            sa.hci_dev = dev_id as u16;
            sa.hci_channel = HCI_CHANNEL_RAW;

            if libc::bind(
                sock,
                &sa as *const _ as *const libc::sockaddr,
                std::mem::size_of::<SockaddrHci>() as libc::socklen_t,
            ) < 0
            {
                let err = std::io::Error::last_os_error();
                libc::close(sock);
                return Err(err).context("bind(AF_BLUETOOTH) failed");
            }

            // 1. Set Local Name: HCI_Write_Local_Name (Opcode 0x0C13 = OGF 0x03, OCF 0x0013)
            let mut name_pkt = [0u8; 4 + 248];
            name_pkt[0] = 0x01; // HCI_COMMAND_PKT
            name_pkt[1] = 0x13;
            name_pkt[2] = 0x0C;
            name_pkt[3] = 248; // Parameter length
            let name_bytes = b"GPT Mini";
            name_pkt[4..4 + name_bytes.len()].copy_from_slice(name_bytes);
            let _ = libc::write(
                sock,
                name_pkt.as_ptr() as *const libc::c_void,
                name_pkt.len(),
            );

            // 2. Set Class of Device: Audio/Video Loudspeaker (0x200414)
            // Major Service: Audio (0x200000), Major Device: Audio/Video (0x0400), Minor: Loudspeaker (0x0014)
            let cod_pkt = [0x01, 0x24, 0x0C, 0x03, 0x14, 0x04, 0x20];
            let _ = libc::write(sock, cod_pkt.as_ptr() as *const libc::c_void, cod_pkt.len());

            // 3. Set Extended Inquiry Response (EIR): HCI_Write_Extended_Inquiry_Response (Opcode 0x0C52)
            // Allows iOS and macOS devices to instantly see name "GPT Mini" and Audio Sink profile
            let mut eir_pkt = [0u8; 4 + 1 + 240];
            eir_pkt[0] = 0x01; // HCI_COMMAND_PKT
            eir_pkt[1] = 0x52; // OCF 0x0052
            eir_pkt[2] = 0x0C; // OGF 0x03
            eir_pkt[3] = 241; // param len: 1 (fec) + 240 (eir data)
            eir_pkt[4] = 0x00; // FEC required: 0

            // EIR Data:
            // Tag 1: Complete Local Name (Type 0x09)
            let eir_data = &mut eir_pkt[5..5 + 240];
            eir_data[0] = 1 + name_bytes.len() as u8;
            eir_data[1] = 0x09; // Complete Local Name
            eir_data[2..2 + name_bytes.len()].copy_from_slice(name_bytes);
            let offset = 2 + name_bytes.len();

            // Tag 2: Complete List of 16-bit Service Class UUIDs (Type 0x03)
            // 0x110B (Audio Sink), 0x110E (AVRCP Target)
            eir_data[offset] = 1 + 4;
            eir_data[offset + 1] = 0x03; // Complete 16-bit Service Class UUIDs
            eir_data[offset + 2] = 0x0B; // 0x110B Audio Sink (low byte)
            eir_data[offset + 3] = 0x11; // (high byte)
            eir_data[offset + 4] = 0x0E; // 0x110E AVRCP (low byte)
            eir_data[offset + 5] = 0x11; // (high byte)

            let _ = libc::write(sock, eir_pkt.as_ptr() as *const libc::c_void, eir_pkt.len());

            // 4. Enable Page Scan & Inquiry Scan via kernel ioctl HCISETSCAN (piscan mode)
            let mut dr = HciDevReq {
                dev_id: dev_id as u16,
                dev_opt: SCAN_PAGE | SCAN_INQUIRY,
            };
            let ret = libc::ioctl(sock, HCISETSCAN as _, &mut dr);
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                warn!("ioctl(HCISETSCAN) returned error: {err:#}");
            }

            // Also write direct HCI_Write_Scan_Enable for controllers requiring explicit packet
            let scan_pkt = [0x01, 0x1A, 0x0C, 0x01, 0x03];
            let _ = libc::write(
                sock,
                scan_pkt.as_ptr() as *const libc::c_void,
                scan_pkt.len(),
            );

            // Allow UART transmission to complete before closing socket
            std::thread::sleep(std::time::Duration::from_millis(100));

            // Query device status to verify flags
            let mut dev_info: HciDevInfo = std::mem::zeroed();
            dev_info.dev_id = dev_id as u16;
            let info_ret = libc::ioctl(sock, HCIGETDEVINFO as _, &mut dev_info);
            libc::close(sock);

            if info_ret >= 0 {
                let flags = dev_info.flags;
                let is_up = (flags & (1 << 0)) != 0;
                let is_running = (flags & (1 << 2)) != 0;
                let is_pscan = (flags & (1 << 3)) != 0;
                let is_iscan = (flags & (1 << 4)) != 0;
                println!(
                    "hci{dev_id}: Scan mode active: Flags={flags:#010X} (UP={is_up}, RUNNING={is_running}, PSCAN={is_pscan}, ISCAN={is_iscan})"
                );
                info!(
                    dev_id,
                    flags = format!("{:#010X}", flags),
                    is_up,
                    is_running,
                    is_pscan,
                    is_iscan,
                    "Bluetooth scan mode active on GPT Mini"
                );
            }

            info!(
                dev_id,
                "Bluetooth discoverable & connectable mode enabled for GPT Mini"
            );
            Ok("Колонка GPT Mini доступна для подключения по Bluetooth".to_string())
        }
    }
}
