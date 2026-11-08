//! Linux wpa_supplicant driver and parser.
//!
//! Handles scanning and network configuration via wpa_cli stdin
//! without exposing Wi-Fi credentials in process command-line arguments.

use super::portal::{BoxFuture, ConnectRequest, ScannedNetwork, WifiBackend};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::info;

pub struct WpaSupplicantBackend {
    interface: String,
}

impl WpaSupplicantBackend {
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
        }
    }
}

impl WifiBackend for WpaSupplicantBackend {
    fn scan_networks(&self) -> BoxFuture<'static, Result<Vec<ScannedNetwork>>> {
        let iface = self.interface.clone();
        Box::pin(async move {
            // Trigger scan
            let _ = Command::new("wpa_cli")
                .args(["-i", &iface, "scan"])
                .output()
                .await;

            // Wait brief moment for scan results to populate
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let output = Command::new("wpa_cli")
                .args(["-i", &iface, "scan_results"])
                .output()
                .await
                .context("Failed to run wpa_cli scan_results")?;

            if !output.status.success() {
                bail!("wpa_cli scan_results exited with {}", output.status);
            }

            let text = String::from_utf8_lossy(&output.stdout);
            Ok(parse_wpa_scan_results(&text))
        })
    }

    fn connect_network(&self, req: ConnectRequest) -> BoxFuture<'static, Result<()>> {
        let iface = self.interface.clone();
        Box::pin(async move {
            info!(ssid = %req.ssid, "Applying Wi-Fi configuration via wpa_cli");

            // 1. Add new network
            let add_out = Command::new("wpa_cli")
                .args(["-i", &iface, "add_network"])
                .output()
                .await
                .context("Failed to add_network")?;

            let net_id_str = String::from_utf8_lossy(&add_out.stdout).trim().to_string();
            let net_id: u32 = net_id_str.parse().with_context(|| {
                format!("wpa_cli add_network returned invalid id: {net_id_str}")
            })?;

            // 2. Prepare interactive script for stdin
            let script = format_wpa_connect_script(net_id, &req);

            let mut child = Command::new("wpa_cli")
                .args(["-i", &iface])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("Failed to spawn interactive wpa_cli")?;

            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(script.as_bytes()).await?;
                stdin.flush().await?;
            }

            let status = child.wait().await?;
            if !status.success() {
                bail!("wpa_cli configuration failed with status {status}");
            }

            Ok(())
        })
    }

    fn enable_soft_ap(&self, ssid: &str) -> BoxFuture<'static, Result<()>> {
        let iface = self.interface.clone();
        let ssid = ssid.to_string();
        Box::pin(async move {
            info!(interface = %iface, %ssid, "Enabling SoftAP mode via wpa_cli");
            let add_out = Command::new("wpa_cli")
                .args(["-i", &iface, "add_network"])
                .output()
                .await
                .context("Failed to add_network for SoftAP")?;

            let net_id_str = String::from_utf8_lossy(&add_out.stdout).trim().to_string();
            let net_id: u32 = net_id_str.parse().with_context(|| {
                format!("wpa_cli add_network returned invalid id: {net_id_str}")
            })?;

            let mut script = String::new();
            let escaped_ssid = ssid.replace('\\', "\\\\").replace('"', "\\\"");
            script.push_str(&format!("set_network {net_id} ssid \"{escaped_ssid}\"\n"));
            script.push_str(&format!("set_network {net_id} mode 2\n"));
            script.push_str(&format!("set_network {net_id} key_mgmt NONE\n"));
            script.push_str(&format!("set_network {net_id} frequency 2412\n"));
            script.push_str(&format!("enable_network {net_id}\n"));
            script.push_str(&format!("select_network {net_id}\n"));
            script.push_str("quit\n");

            let mut child = Command::new("wpa_cli")
                .args(["-i", &iface])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("Failed to spawn interactive wpa_cli for SoftAP")?;

            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(script.as_bytes()).await?;
                stdin.flush().await?;
            }

            let _ = child.wait().await;

            // Assign static IP for Captive Portal (192.168.4.1/24)
            let _ = Command::new("ifconfig")
                .args([&iface, "192.168.4.1", "netmask", "255.255.255.0", "up"])
                .output()
                .await;

            Ok(())
        })
    }

    fn disable_soft_ap(&self) -> BoxFuture<'static, Result<()>> {
        let iface = self.interface.clone();
        Box::pin(async move {
            info!(interface = %iface, "Disabling SoftAP mode and requesting DHCP");
            let _ = Command::new("udhcpc")
                .args(["-i", &iface, "-n", "-q"])
                .output()
                .await;
            Ok(())
        })
    }
}

/// Parses the tab-separated table from `wpa_cli scan_results`.
/// Format: `bssid / frequency / signal level / flags / ssid`
pub fn parse_wpa_scan_results(output: &str) -> Vec<ScannedNetwork> {
    let mut by_ssid: HashMap<String, ScannedNetwork> = HashMap::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("bssid") {
            continue;
        }

        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }

        let signal: i32 = parts[2].parse().unwrap_or(-100);
        let flags = parts[3];
        let ssid = parts[4].trim().to_string();

        if ssid.is_empty() {
            continue; // Ignore hidden SSIDs
        }

        let security = if flags.contains("WPA3") || flags.contains("SAE") {
            "WPA3".to_string()
        } else if flags.contains("WPA2") {
            "WPA2".to_string()
        } else if flags.contains("WPA") {
            "WPA".to_string()
        } else {
            "Open".to_string()
        };

        // If duplicate SSID found, keep the one with stronger signal
        match by_ssid.get(&ssid) {
            Some(existing) if existing.signal_strength >= signal => {}
            _ => {
                by_ssid.insert(
                    ssid.clone(),
                    ScannedNetwork {
                        ssid,
                        signal_strength: signal,
                        security,
                    },
                );
            }
        }
    }

    let mut list: Vec<ScannedNetwork> = by_ssid.into_values().collect();
    // Sort descending by signal strength
    list.sort_by_key(|a| std::cmp::Reverse(a.signal_strength));
    list
}

/// Formats commands to send into wpa_cli stdin.
/// Escapes quotes in SSID and PSK to prevent injection.
pub fn format_wpa_connect_script(net_id: u32, req: &ConnectRequest) -> String {
    let escaped_ssid = req.ssid.replace('\\', "\\\\").replace('"', "\\\"");
    let mut script = String::new();

    script.push_str(&format!("set_network {net_id} ssid \"{escaped_ssid}\"\n"));

    if req.psk.trim().is_empty() {
        script.push_str(&format!("set_network {net_id} key_mgmt NONE\n"));
    } else {
        let escaped_psk = req.psk.replace('\\', "\\\\").replace('"', "\\\"");
        script.push_str(&format!("set_network {net_id} psk \"{escaped_psk}\"\n"));
    }

    script.push_str(&format!("enable_network {net_id}\n"));
    script.push_str(&format!("select_network {net_id}\n"));
    script.push_str("save_config\n");
    script.push_str("quit\n");

    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wpa_scan_results_deduplication_and_sorting() {
        let raw = "\
bssid / frequency / signal level / flags / ssid
20:e8:82:11:22:33\t2437\t-65\t[WPA2-PSK-CCMP][ESS]\tMyHomeNetwork
20:e8:82:11:22:34\t5180\t-45\t[WPA2-PSK-CCMP][ESS]\tMyHomeNetwork
e4:8d:8c:55:66:77\t2412\t-80\t[WPA3-SAE-CCMP][ESS]\tNeighbor-5G
12:34:56:78:9a:bc\t2462\t-70\t[ESS]\tFreeWiFi
00:11:22:33:44:55\t2412\t-90\t[WPA2-PSK-CCMP][ESS]\t
";

        let results = parse_wpa_scan_results(raw);
        assert_eq!(results.len(), 3);

        // First should be MyHomeNetwork (-45 dBm, strong signal wins)
        assert_eq!(results[0].ssid, "MyHomeNetwork");
        assert_eq!(results[0].signal_strength, -45);
        assert_eq!(results[0].security, "WPA2");

        // Second should be FreeWiFi (-70 dBm)
        assert_eq!(results[1].ssid, "FreeWiFi");
        assert_eq!(results[1].security, "Open");

        // Third should be Neighbor-5G (-80 dBm)
        assert_eq!(results[2].ssid, "Neighbor-5G");
        assert_eq!(results[2].security, "WPA3");
    }

    #[test]
    fn test_format_wpa_connect_script_escaping() {
        let req = ConnectRequest {
            ssid: "My \"Secret\" Wi-Fi".into(),
            psk: "P@$$w\"ord\\123".into(),
        };

        let script = format_wpa_connect_script(2, &req);
        assert!(script.contains("set_network 2 ssid \"My \\\"Secret\\\" Wi-Fi\""));
        assert!(script.contains("set_network 2 psk \"P@$$w\\\"ord\\\\123\""));
        assert!(script.contains("enable_network 2"));
        assert!(script.contains("select_network 2"));
        assert!(script.contains("save_config"));
    }

    #[test]
    fn test_format_wpa_connect_script_open_network() {
        let req = ConnectRequest {
            ssid: "OpenCafe".into(),
            psk: "".into(),
        };

        let script = format_wpa_connect_script(0, &req);
        assert!(script.contains("set_network 0 ssid \"OpenCafe\""));
        assert!(script.contains("set_network 0 key_mgmt NONE"));
        assert!(!script.contains("psk"));
    }
}
