//! Broadcom BCM43430 UART Firmware Loader & HCI Line Discipline Attacher.
//!
//! Replaces BlueZ `hciattach -s 115200 /dev/ttyS1 bcm43xx 2000000 flow`
//! in pure Rust for Station Mini 1 (Amlogic A113X, Linux 4.9.68).
//!
//! Sequence:
//! 1. Open serial port (default: `/dev/ttyS1`) at 115,200 baud, 8N1, CRTSCTS.
//! 2. Send HCI_Reset (0x0C03).
//! 3. Send BCM_Download_Minidriver (0xFC2E).
//! 4. Stream all records from `/etc/bluetooth/BCM43430.hcd`.
//! 5. Send post-patch HCI_Reset (0x0C03).
//! 6. Send BCM_Set_Speed (0xFC18) for 2,000,000 baud (0x001E8480).
//! 7. Switch local UART to 2,000,000 baud.
//! 8. Set Linux line discipline to N_HCI (15) with protocol HCI_UART_H4 (0).
//! 9. Keep device attached until SIGINT/SIGTERM, then detach cleanly.

use anyhow::{bail, Context, Result};
use mini_assistant::bluetooth::hcd::parse_hcd;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct BcmConfig {
    pub device: PathBuf,
    pub firmware: PathBuf,
    pub init_speed: u32,
    pub speed: u32,
    pub dry_run: bool,
    pub check: bool,
}

impl Default for BcmConfig {
    fn default() -> Self {
        Self {
            device: PathBuf::from("/dev/ttyS1"),
            firmware: PathBuf::from("/etc/bluetooth/BCM43430.hcd"),
            init_speed: 115_200,
            speed: 2_000_000,
            dry_run: false,
            check: false,
        }
    }
}

pub fn parse_cli_args<I, T>(iter: I) -> Result<Option<BcmConfig>>
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let mut args = iter.into_iter();
    let _prog = args.next();
    let mut cfg = BcmConfig::default();

    while let Some(arg) = args.next() {
        match arg.as_ref() {
            "-h" | "--help" => {
                println!(
                    "Usage: bcm_attach [OPTIONS]\n\n\
                     Options:\n\
                       -d, --device <PATH>     Serial device (default: /dev/ttyS1)\n\
                       -f, --firmware <PATH>   Broadcom .hcd firmware file (default: /etc/bluetooth/BCM43430.hcd)\n\
                       -s, --speed <BAUD>      Target baud rate (default: 2000000)\n\
                       -i, --init-speed <BAUD> Initial baud rate (default: 115200)\n\
                       --check                 Verify firmware file syntax and exit\n\
                       --dry-run               Simulate loading without opening serial port\n\
                       -v, --version           Print version\n\
                       -h, --help              Print help"
                );
                return Ok(None);
            }
            "-v" | "--version" => {
                println!("bcm_attach 0.1.0");
                return Ok(None);
            }
            "--check" => cfg.check = true,
            "--dry-run" => cfg.dry_run = true,
            "-d" | "--device" => {
                let val = args.next().context("--device requires a path argument")?;
                cfg.device = PathBuf::from(val.as_ref());
            }
            "-f" | "--firmware" => {
                let val = args.next().context("--firmware requires a path argument")?;
                cfg.firmware = PathBuf::from(val.as_ref());
            }
            "-s" | "--speed" => {
                let val = args.next().context("--speed requires a baud rate")?;
                cfg.speed = val.as_ref().parse().context("Invalid speed baud rate")?;
            }
            "-i" | "--init-speed" => {
                let val = args.next().context("--init-speed requires a baud rate")?;
                cfg.init_speed = val
                    .as_ref()
                    .parse()
                    .context("Invalid init-speed baud rate")?;
            }
            unknown => bail!("Unknown option: {unknown}"),
        }
    }

    Ok(Some(cfg))
}

pub fn verify_firmware_file(path: &Path) -> Result<usize> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read firmware from {}", path.display()))?;
    let records = parse_hcd(&bytes)
        .with_context(|| format!("Invalid .hcd firmware format in {}", path.display()))?;
    Ok(records.len())
}

#[cfg(target_os = "linux")]
mod linux_uart {
    use super::*;
    use mini_assistant::bluetooth::hcd::{
        format_bcm_minidriver, format_bcm_set_speed, format_hci_reset, parse_command_complete,
        BCM_CMD_DOWNLOAD_MINIDRIVER, BCM_CMD_SET_BAUD_RATE, HCI_CMD_RESET,
    };
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::time::Duration;
    use tracing::info;

    const N_HCI: libc::c_int = 15;
    const HCI_UART_H4: libc::c_int = 0;
    // _IOW('U', 200, int) = 0x400455C8
    const HCIUARTSETPROTO: libc::c_ulong = 0x400455C8;

    fn to_termios_baud(baud: u32) -> Result<libc::speed_t> {
        match baud {
            115_200 => Ok(libc::B115200),
            230_400 => Ok(libc::B230400),
            460_800 => Ok(libc::B460800),
            500_000 => Ok(libc::B500000),
            576_000 => Ok(libc::B576000),
            921_600 => Ok(libc::B921600),
            1_000_000 => Ok(libc::B1000000),
            1_500_000 => Ok(libc::B1500000),
            2_000_000 => Ok(libc::B2000000),
            3_000_000 => Ok(libc::B3000000),
            4_000_000 => Ok(libc::B4000000),
            other => bail!("Unsupported baud rate: {other}"),
        }
    }

    fn set_serial_params(fd: RawFd, baud: u32) -> Result<()> {
        let speed = to_termios_baud(baud)?;
        unsafe {
            let mut tios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut tios) != 0 {
                return Err(std::io::Error::last_os_error()).context("tcgetattr failed");
            }

            libc::cfmakeraw(&mut tios);

            // 8N1 + CREAD + CLOCAL + CRTSCTS (hardware flow control)
            tios.c_cflag |= libc::CS8 | libc::CREAD | libc::CLOCAL | libc::CRTSCTS;
            tios.c_cc[libc::VMIN] = 0;
            tios.c_cc[libc::VTIME] = 20; // 2.0s timeout

            libc::cfsetispeed(&mut tios, speed);
            libc::cfsetospeed(&mut tios, speed);

            if libc::tcsetattr(fd, libc::TCSANOW, &tios) != 0 {
                return Err(std::io::Error::last_os_error()).context("tcsetattr failed");
            }
            libc::tcflush(fd, libc::TCIOFLUSH);
        }
        Ok(())
    }

    fn write_exact(fd: RawFd, buf: &[u8]) -> Result<()> {
        let mut total = 0;
        while total < buf.len() {
            let res = unsafe {
                libc::write(
                    fd,
                    buf[total..].as_ptr() as *const libc::c_void,
                    buf.len() - total,
                )
            };
            if res < 0 {
                return Err(std::io::Error::last_os_error()).context("write to UART failed");
            }
            total += res as usize;
        }
        Ok(())
    }

    fn read_hci_event(fd: RawFd, expected_opcode: u16) -> Result<()> {
        let mut buf = [0u8; 260];
        let mut total = 0;
        let start = std::time::Instant::now();

        // Read at least 7 bytes for Command Complete header
        while total < 7 {
            if start.elapsed() > Duration::from_secs(2) {
                bail!("Timeout waiting for HCI Command Complete event (opcode {expected_opcode:#06X})");
            }
            let res = unsafe {
                libc::read(
                    fd,
                    buf[total..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - total,
                )
            };
            if res > 0 {
                total += res as usize;
            } else if res < 0 {
                return Err(std::io::Error::last_os_error()).context("read from UART failed");
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        let event_len = buf[2] as usize;
        let target_len = 3 + event_len;

        while total < target_len && total < buf.len() {
            if start.elapsed() > Duration::from_secs(2) {
                bail!("Timeout reading complete HCI event payload");
            }
            let res = unsafe {
                libc::read(
                    fd,
                    buf[total..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - total,
                )
            };
            if res > 0 {
                total += res as usize;
            } else if res < 0 {
                return Err(std::io::Error::last_os_error()).context("read from UART failed");
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        parse_command_complete(&buf[..total], expected_opcode)
    }

    fn bring_up_hci(dev_id: libc::c_int) -> Result<String> {
        const AF_BLUETOOTH: libc::c_int = 31;
        const BTPROTO_HCI: libc::c_int = 1;
        const HCIDEVUP: libc::c_ulong = 0x400448C9;
        const HCIGETDEVINFO: libc::c_ulong = 0x800448D3;

        unsafe {
            let sock = libc::socket(AF_BLUETOOTH, libc::SOCK_RAW, BTPROTO_HCI);
            if sock < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Failed to open AF_BLUETOOTH raw socket");
            }

            // 1. Bring interface UP (ignore if already up)
            if libc::ioctl(sock, HCIDEVUP as _, dev_id as libc::c_ulong) < 0 {
                let err = std::io::Error::last_os_error();
                let code = err.raw_os_error().unwrap_or(0);
                if code != 114 && code != 16 {
                    libc::close(sock);
                    return Err(err).context("ioctl(HCIDEVUP) failed");
                }
            }

            // 2. Query device info
            #[repr(C)]
            struct DevInfoBuffer {
                bytes: [u8; 92],
            }
            let mut buf = DevInfoBuffer { bytes: [0u8; 92] };
            buf.bytes[0] = (dev_id & 0xFF) as u8;
            buf.bytes[1] = ((dev_id >> 8) & 0xFF) as u8;

            let ret = libc::ioctl(sock, HCIGETDEVINFO as _, &mut buf);
            libc::close(sock);

            if ret < 0 {
                return Err(std::io::Error::last_os_error()).context("ioctl(HCIGETDEVINFO) failed");
            }

            let addr = format!(
                "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                buf.bytes[15],
                buf.bytes[14],
                buf.bytes[13],
                buf.bytes[12],
                buf.bytes[11],
                buf.bytes[10]
            );
            let flags = u32::from_ne_bytes(buf.bytes[16..20].try_into().unwrap_or_default());
            let is_up = (flags & (1 << 0)) != 0;
            let is_running = (flags & (1 << 2)) != 0;
            let is_pscan = (flags & (1 << 3)) != 0;
            let is_iscan = (flags & (1 << 4)) != 0;

            Ok(format!(
                "hci{dev_id}: BD_ADDR={addr}, Flags=0x{flags:08X} (UP={is_up}, RUNNING={is_running}, PSCAN={is_pscan}, ISCAN={is_iscan})"
            ))
        }
    }

    pub async fn load_firmware_and_attach(cfg: &BcmConfig) -> Result<()> {
        info!(
            device = %cfg.device.display(),
            firmware = %cfg.firmware.display(),
            speed = cfg.speed,
            "Loading Broadcom BCM43430 firmware"
        );

        let fw_bytes = std::fs::read(&cfg.firmware)
            .with_context(|| format!("Failed to read {}", cfg.firmware.display()))?;
        let records = parse_hcd(&fw_bytes)
            .with_context(|| format!("Invalid HCD records in {}", cfg.firmware.display()))?;

        // Optional hardware rfkill reset if available (toggles BT core)
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .open("/sys/class/rfkill/rfkill0/state")
        {
            use std::io::Write;
            let _ = f.write_all(b"0\n");
            let _ = f.flush();
            std::thread::sleep(Duration::from_millis(100));
            let _ = f.write_all(b"1\n");
            let _ = f.flush();
            std::thread::sleep(Duration::from_millis(200));
            info!("Toggled rfkill0 to reset Broadcom Bluetooth core");
        }

        // Ensure hci_uart kernel module is loaded
        let _ = std::process::Command::new("modprobe")
            .arg("hci_uart")
            .status();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&cfg.device)
            .with_context(|| format!("Failed to open serial device {}", cfg.device.display()))?;

        let fd = file.as_raw_fd();

        // Clear non-blocking
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }

        // 1. Initial speed 115200 baud
        set_serial_params(fd, cfg.init_speed)?;
        std::thread::sleep(Duration::from_millis(50));

        // 2. Initial HCI Reset
        write_exact(fd, &format_hci_reset())?;
        read_hci_event(fd, HCI_CMD_RESET)?;

        // 3. Download Minidriver command
        write_exact(fd, &format_bcm_minidriver())?;
        read_hci_event(fd, BCM_CMD_DOWNLOAD_MINIDRIVER)?;
        std::thread::sleep(Duration::from_millis(50));

        // 4. Send all HCD records
        info!(
            records = records.len(),
            "Streaming firmware patch records to BCM43430"
        );
        for (i, rec) in records.iter().enumerate() {
            write_exact(fd, &rec.to_hci_packet())?;
            read_hci_event(fd, rec.opcode).with_context(|| {
                format!(
                    "HCD record {i}/{} (opcode {:#06X}) rejected",
                    records.len(),
                    rec.opcode
                )
            })?;
        }
        std::thread::sleep(Duration::from_millis(50));

        // 5. Post-patch HCI Reset
        write_exact(fd, &format_hci_reset())?;
        read_hci_event(fd, HCI_CMD_RESET)?;

        // 6. Set high baud rate (e.g. 2,000,000 baud)
        write_exact(fd, &format_bcm_set_speed(cfg.speed))?;
        read_hci_event(fd, BCM_CMD_SET_BAUD_RATE)?;

        // 7. Switch local UART to high baud rate
        set_serial_params(fd, cfg.speed)?;
        std::thread::sleep(Duration::from_millis(50));
        info!(
            speed = cfg.speed,
            "BCM43430 operational baud rate established"
        );

        // 8. Attach Linux line discipline N_HCI (15) with proto HCI_UART_H4 (0)
        unsafe {
            let mut ldisc = N_HCI;
            if libc::ioctl(fd, libc::TIOCSETD as _, &mut ldisc) < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("ioctl(TIOCSETD, N_HCI) failed");
            }

            if libc::ioctl(fd, HCIUARTSETPROTO as _, HCI_UART_H4 as libc::c_ulong) < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("ioctl(HCIUARTSETPROTO, HCI_UART_H4) failed");
            }
        }

        info!("Successfully attached BCM43430 to kernel HCI H4 discipline (hci0 created)");

        // 9. Bring UP hci0 and query BD_ADDR
        std::thread::sleep(Duration::from_millis(150));
        match bring_up_hci(0) {
            Ok(info_str) => {
                info!(info = %info_str, "HCI device initialized and brought UP");
                println!("{info_str}");

                // Initialize discoverable & connectable piscan mode with name "GPT Mini"
                match mini_assistant::bluetooth::linux_bt::set_discoverable_linux(0) {
                    Ok(msg) => {
                        info!(%msg, "Bluetooth discoverable piscan mode enabled");
                        println!("{msg}");
                    }
                    Err(e) => {
                        tracing::warn!("Could not set Bluetooth discoverable mode: {e:#}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Could not bring UP hci0 via ioctl: {e:#}");
            }
        }

        println!("BCM43430 attached as hci0. Press Ctrl+C to terminate and detach.");

        // Wait for shutdown signal (SIGINT or SIGTERM)
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("Failed to register SIGTERM handler")?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }

        // Clean detach: reset line discipline to TTY (0)
        info!("Detaching BCM43430 from kernel line discipline");
        unsafe {
            let mut ldisc = 0; // N_TTY
            let _ = libc::ioctl(fd, libc::TIOCSETD as _, &mut ldisc);
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();
    let Some(cfg) = parse_cli_args(args)? else {
        return Ok(());
    };

    if cfg.check {
        let count = verify_firmware_file(&cfg.firmware)?;
        println!(
            "OK: {} valid HCD records found in {}",
            count,
            cfg.firmware.display()
        );
        return Ok(());
    }

    if cfg.dry_run {
        println!(
            "Dry-run: device={}, firmware={}, speed={}",
            cfg.device.display(),
            cfg.firmware.display(),
            cfg.speed
        );
        if cfg.firmware.exists() {
            let count = verify_firmware_file(&cfg.firmware)?;
            println!("Dry-run verified: {} records ready to load", count);
        } else {
            println!(
                "Dry-run: firmware file {} does not exist on this host",
                cfg.firmware.display()
            );
        }
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        linux_uart::load_firmware_and_attach(&cfg).await?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        println!(
            "Notice: Line discipline attachment (TIOCSETD, N_HCI) is a Linux-specific kernel feature.\n\
             On non-Linux hosts, use --check or --dry-run."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cli_args_defaults() {
        let args = ["bcm_attach"];
        let cfg = parse_cli_args(args).unwrap().unwrap();
        assert_eq!(cfg.device, PathBuf::from("/dev/ttyS1"));
        assert_eq!(cfg.firmware, PathBuf::from("/etc/bluetooth/BCM43430.hcd"));
        assert_eq!(cfg.init_speed, 115_200);
        assert_eq!(cfg.speed, 2_000_000);
        assert!(!cfg.dry_run);
        assert!(!cfg.check);
    }

    #[test]
    fn test_parse_cli_args_custom() {
        let args = [
            "bcm_attach",
            "-d",
            "/dev/ttyUSB0",
            "-f",
            "/tmp/test.hcd",
            "-s",
            "3000000",
            "--dry-run",
        ];
        let cfg = parse_cli_args(args).unwrap().unwrap();
        assert_eq!(cfg.device, PathBuf::from("/dev/ttyUSB0"));
        assert_eq!(cfg.firmware, PathBuf::from("/tmp/test.hcd"));
        assert_eq!(cfg.speed, 3_000_000);
        assert!(cfg.dry_run);
    }

    #[test]
    fn test_verify_firmware_file_synthetic() {
        let path = std::env::temp_dir().join(format!("test_bcm_{}.hcd", std::process::id()));
        // Opcode 0xFC4C, len 2, params [1, 2]
        std::fs::write(&path, [0x4C, 0xFC, 0x02, 0x01, 0x02]).unwrap();

        let count = verify_firmware_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(count, 1);
    }
}
