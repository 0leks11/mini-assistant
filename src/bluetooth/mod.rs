//! Bluetooth and Broadcom BCM43430 UART loader subsystem.

pub mod hcd;

#[allow(unused_imports)]
use anyhow::{Context, Result};
#[allow(unused_imports)]
use tracing::info;

/// Enable Bluetooth discoverable and connectable mode on `hci0`.
/// Sets the local device name to "GPT Mini" and enables inquiry + page scanning.
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
mod linux_bt {
    #[allow(unused_imports)]
use anyhow::{Context, Result};
    use tracing::info;

    const AF_BLUETOOTH: libc::c_int = 31;
    const BTPROTO_HCI: libc::c_int = 1;
    const HCI_CHANNEL_RAW: u16 = 0;

    #[repr(C)]
    struct SockaddrHci {
        hci_family: libc::sa_family_t,
        hci_dev: u16,
        hci_channel: u16,
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
            // Packet: [HCI_COMMAND_PKT (0x01), Opcode low (0x13), Opcode high (0x0C), Length (248), 248 bytes name]
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
            // HCI_Write_Class_of_Device: Opcode 0x0C24
            let cod_pkt = [0x01, 0x24, 0x0C, 0x03, 0x14, 0x04, 0x20];
            let _ = libc::write(sock, cod_pkt.as_ptr() as *const libc::c_void, cod_pkt.len());

            // 3. Write Scan Enable: 0x03 (Inquiry Scan | Page Scan = Discoverable & Connectable)
            // HCI_Write_Scan_Enable: Opcode 0x0C1A
            let scan_pkt = [0x01, 0x1A, 0x0C, 0x01, 0x03];
            let ret = libc::write(
                sock,
                scan_pkt.as_ptr() as *const libc::c_void,
                scan_pkt.len(),
            );
            libc::close(sock);

            if ret < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Failed to send HCI_Write_Scan_Enable");
            }

            info!(
                dev_id,
                "Bluetooth discoverable & connectable mode enabled for GPT Mini"
            );
            Ok("Колонка GPT Mini доступна для подключения по Bluetooth".to_string())
        }
    }
}
