//! VL53L0X Time-of-Flight Proximity Sensor Driver for GPT Mini.
//!
//! Controls playback volume via hand proximity over the top lid:
//! - Hand close (40–50 mm)  → lower volume (level 1–2)
//! - Hand raised (250–300 mm) → higher volume (level 9–10)
//! - Hand absent (> 350 mm) → idle (keeps current volume)
//!
//! Kernel driver `stmvl53l0x` exposes `/dev/input/event0` with ABS events:
//! - `ev_type = 3` (EV_ABS), `code = 8` (distance in micrometers, `val / 1000 = mm`).

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::{FromRawFd, RawFd};
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct InputEvent {
    tv_sec: u32,
    tv_usec: u32,
    ev_type: u16,
    code: u16,
    value: i32,
}

const EV_ABS: u16 = 0x03;
const EV_SYN: u16 = 0x00;
const CODE_DISTANCE_UM: u16 = 8;

/// Map distance in millimeters to volume level (1..=10).
pub fn distance_to_level(dist_mm: u32) -> Option<u8> {
    const MIN_MM: u32 = 40;
    const MAX_MM: u32 = 300;
    const IDLE_THRESHOLD_MM: u32 = 350;

    if dist_mm > IDLE_THRESHOLD_MM {
        return None; // Hand not present
    }

    if dist_mm <= MIN_MM {
        return Some(1);
    }
    if dist_mm >= MAX_MM {
        return Some(10);
    }

    let range = MAX_MM - MIN_MM;
    let offset = dist_mm - MIN_MM;
    let level = 1 + (offset * 9 / range) as u8;
    Some(level.clamp(1, 10))
}

/// Enable the hardware sensor via sysfs.
pub fn enable_hardware_sensor() {
    let sysfs_path = "/sys/devices/virtual/input/input0/enable_ps_sensor";
    if let Ok(mut file) = OpenOptions::new().write(true).open(sysfs_path) {
        let _ = file.write_all(b"1\n");
        info!("VL53L0 proximity sensor enabled via sysfs");
    }
}

/// Spawn the background gesture-volume reader task.
pub fn spawn_vl53_volume_task(
    soloist_client: Option<crate::spotify::SoloistClient>,
    ducker: Option<std::sync::Arc<crate::spotify::MusicDucker>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        enable_hardware_sensor();

        let path = "/dev/input/event0";
        let fd: RawFd = match unsafe {
            let cstr = std::ffi::CString::new(path).unwrap();
            libc::open(cstr.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK)
        } {
            fd if fd >= 0 => fd,
            _ => {
                debug!("VL53L0: cannot open {path}, gesture volume task stopped");
                return;
            }
        };

        info!("VL53L0 proximity gesture volume reader active on {path}");

        #[allow(deprecated)]
        let async_fd = match AsyncFd::new(unsafe { std::fs::File::from_raw_fd(fd) }) {
            Ok(f) => f,
            Err(e) => {
                warn!("VL53L0: AsyncFd error: {e}");
                return;
            }
        };

        let event_size = std::mem::size_of::<InputEvent>();
        let mut buf = vec![0u8; event_size];
        let mut last_reported_level: u8 = crate::audio::volume::current_level();
        let mut stable_candidate: Option<u8> = None;
        let mut stable_count: usize = 0;
        let mut last_event_dist: u32 = 0;

        loop {
            match async_fd.readable().await {
                Ok(mut guard) => {
                    use std::io::Read;
                    match guard.get_inner().read_exact(&mut buf) {
                        Ok(_) => {
                            let ev = unsafe {
                                std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent)
                            };
                            if ev.ev_type == EV_ABS && ev.code == CODE_DISTANCE_UM && ev.value >= 0
                            {
                                // Distance reported in micrometers: convert to mm
                                last_event_dist = (ev.value / 1000) as u32;
                            } else if ev.ev_type == EV_SYN {
                                // Process measurement at SYN_REPORT
                                if let Some(new_level) = distance_to_level(last_event_dist) {
                                    if stable_candidate == Some(new_level) {
                                        stable_count += 1;
                                    } else {
                                        stable_candidate = Some(new_level);
                                        stable_count = 1;
                                    }

                                    // Require 2 consecutive measurements (~60ms) to commit volume change
                                    if stable_count >= 2 && new_level != last_reported_level {
                                        last_reported_level = new_level;
                                        crate::audio::volume::set_volume_level(new_level);
                                        if let Some(ref d) = ducker {
                                            d.cancel();
                                        }
                                        info!(
                                            new_level,
                                            dist_mm = last_event_dist,
                                            "VL53L0 gesture: volume changed"
                                        );

                                        // Sync to Soloist if available
                                        if let Some(ref soloist) = soloist_client {
                                            let soloist_vol = new_level * 10;
                                            let soloist = soloist.clone();
                                            tokio::spawn(async move {
                                                let _ = soloist.set_volume(soloist_vol).await;
                                            });
                                        }
                                    }
                                } else {
                                    // Hand out of range (idle): reset debounce counters
                                    stable_candidate = None;
                                    stable_count = 0;
                                }
                            }
                            guard.clear_ready();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            guard.clear_ready();
                        }
                        Err(e) => {
                            warn!("VL53L0 read error: {e}");
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                            guard.clear_ready();
                        }
                    }
                }
                Err(e) => {
                    warn!("VL53L0 poll error: {e}");
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance_to_level_mapping() {
        assert_eq!(distance_to_level(30), Some(1));
        assert_eq!(distance_to_level(40), Some(1));
        assert_eq!(distance_to_level(300), Some(10));
        assert_eq!(distance_to_level(350), Some(10));
        assert_eq!(distance_to_level(351), None); // Idle
        assert_eq!(distance_to_level(1000), None); // Idle
    }

    #[test]
    fn test_distance_monotonicity() {
        let mut prev = 1;
        for d in 40..=300 {
            if let Some(lvl) = distance_to_level(d) {
                assert!(lvl >= prev);
                prev = lvl;
            }
        }
        assert_eq!(prev, 10);
    }
}
