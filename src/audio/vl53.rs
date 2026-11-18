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

pub const STEP_MM: u32 = 25;
pub const MIN_GESTURE_MM: u32 = 40;
#[allow(dead_code)]
pub const MAX_GESTURE_MM: u32 = 300;
pub const IDLE_THRESHOLD_MM: u32 = 330;

/// State tracker for relative vertical hand gestures over the proximity sensor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureSession {
    pub in_gesture: bool,
    pub anchor_mm: u32,
    pub current_level: u8,
}

impl GestureSession {
    pub fn new(current_level: u8) -> Self {
        Self {
            in_gesture: false,
            anchor_mm: 0,
            current_level: current_level.clamp(1, 10),
        }
    }

    /// Process a new distance measurement (in mm).
    /// Returns `Some(new_level)` if the volume level changed (meaning click + update volume!),
    /// or `None` if volume did not change.
    /// Also updates `in_gesture`.
    pub fn process_distance(&mut self, dist_mm: u32) -> Option<u8> {
        if dist_mm > IDLE_THRESHOLD_MM {
            self.in_gesture = false;
            self.anchor_mm = 0;
            return None;
        }

        if dist_mm < MIN_GESTURE_MM {
            return None;
        }

        if !self.in_gesture {
            self.in_gesture = true;
            self.anchor_mm = dist_mm;
            return None; // Initial entry anchors the position, no volume change yet
        }

        // Relative delta: positive = hand raised higher (up), negative = hand lowered (down)
        if dist_mm >= self.anchor_mm + STEP_MM {
            let steps = (dist_mm - self.anchor_mm) / STEP_MM;
            let mut changed = false;
            for _ in 0..steps {
                if self.current_level < 10 {
                    self.current_level += 1;
                    self.anchor_mm += STEP_MM;
                    changed = true;
                } else {
                    // Reached ceiling 10: clamp anchor so descending responds immediately without deadzone
                    self.anchor_mm = dist_mm;
                    break;
                }
            }
            if changed {
                return Some(self.current_level);
            }
        } else if self.anchor_mm >= dist_mm + STEP_MM {
            let steps = (self.anchor_mm - dist_mm) / STEP_MM;
            let mut changed = false;
            for _ in 0..steps {
                if self.current_level > 1 {
                    self.current_level -= 1;
                    self.anchor_mm -= STEP_MM;
                    changed = true;
                } else {
                    // Reached floor 1: clamp anchor
                    self.anchor_mm = dist_mm;
                    break;
                }
            }
            if changed {
                return Some(self.current_level);
            }
        }

        None
    }
}

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

/// Spawn the background gesture-volume reader task with backward-compatible defaults.
#[allow(dead_code)]
pub fn spawn_vl53_volume_task(
    soloist_client: Option<crate::spotify::SoloistClient>,
    ducker: Option<std::sync::Arc<crate::spotify::MusicDucker>>,
) -> tokio::task::JoinHandle<()> {
    spawn_vl53_volume_task_with_indicators(
        soloist_client,
        ducker,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        std::sync::Arc::new(tokio::sync::RwLock::new("azure".to_string())),
    )
}

/// Spawn the background gesture-volume reader task with visual and tactile indicators.
pub fn spawn_vl53_volume_task_with_indicators(
    soloist_client: Option<crate::spotify::SoloistClient>,
    _ducker: Option<std::sync::Arc<crate::spotify::MusicDucker>>,
    gesture_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    led_color: std::sync::Arc<tokio::sync::RwLock<String>>,
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
        let mut session = GestureSession::new(crate::audio::volume::current_level());
        let mut last_event_dist: u32 = 0;
        let mut was_in_gesture = false;
        let mut last_gesture_activity = std::time::Instant::now();
        let mut active_interaction = false;

        loop {
            // Wait up to 400ms for new proximity event; if timed out while in gesture, exit gesture cleanly
            let poll_result = tokio::time::timeout(
                tokio::time::Duration::from_millis(400),
                async_fd.readable(),
            )
            .await;

            match poll_result {
                Ok(Ok(mut guard)) => {
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
                                session.current_level = crate::audio::volume::current_level();
                                if let Some(new_level) = session.process_distance(last_event_dist) {
                                    // Step change detected! Update volume and trigger acoustic "клац"
                                    crate::audio::volume::set_volume_level(new_level);
                                    crate::audio::volume::trigger_volume_click();
                                    active_interaction = true;
                                    last_gesture_activity = std::time::Instant::now();
                                    info!(
                                        new_level,
                                        dist_mm = last_event_dist,
                                        anchor_mm = session.anchor_mm,
                                        "VL53L0 relative gesture: volume changed with tactile click"
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

                                // Update visual indicator state: active only during real volume gesture interaction
                                if active_interaction {
                                    if last_gesture_activity.elapsed() >= std::time::Duration::from_millis(1000)
                                        || last_event_dist > IDLE_THRESHOLD_MM
                                    {
                                        active_interaction = false;
                                        session.in_gesture = false;
                                        session.anchor_mm = last_event_dist;
                                        if was_in_gesture {
                                            was_in_gesture = false;
                                            gesture_active.store(false, std::sync::atomic::Ordering::SeqCst);
                                            crate::web::set_led_hardware("off");
                                            debug!("VL53L0 gesture volume interaction finished; LED turned off");
                                        }
                                    } else if !was_in_gesture {
                                        was_in_gesture = true;
                                        gesture_active.store(true, std::sync::atomic::Ordering::SeqCst);
                                        let cur_color = led_color
                                            .try_read()
                                            .map(|c| c.clone())
                                            .unwrap_or_else(|_| "azure".to_string());
                                        crate::web::set_led_hardware(&cur_color);
                                    }
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
                Ok(Err(e)) => {
                    warn!("VL53L0 poll error: {e}");
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
                Err(_) => {
                    // 400ms timeout expired with no events: hand was swiftly removed
                    if was_in_gesture || active_interaction {
                        active_interaction = false;
                        session.in_gesture = false;
                        session.anchor_mm = 0;
                        was_in_gesture = false;
                        gesture_active.store(false, std::sync::atomic::Ordering::SeqCst);
                        crate::web::set_led_hardware("off");
                        debug!("VL53L0 gesture ended due to inactivity timeout");
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

    #[test]
    fn test_relative_gesture_session_stepping_up_and_down() {
        let mut session = GestureSession::new(7);
        assert!(!session.in_gesture);

        // Hand enters at 150mm: anchors session, no volume change yet
        assert_eq!(session.process_distance(150), None);
        assert!(session.in_gesture);
        assert_eq!(session.anchor_mm, 150);
        assert_eq!(session.current_level, 7);

        // Small jitter < 25mm: no change
        assert_eq!(session.process_distance(145), None);
        assert_eq!(session.current_level, 7);

        // Lower hand to 125mm (-25mm step): level decreases to 6 (1st click)
        assert_eq!(session.process_distance(125), Some(6));
        assert_eq!(session.anchor_mm, 125);
        assert_eq!(session.current_level, 6);

        // Lower hand to 100mm (-25mm step): level decreases to 5 (2nd click)
        assert_eq!(session.process_distance(100), Some(5));
        assert_eq!(session.anchor_mm, 100);
        assert_eq!(session.current_level, 5);

        // Lower hand to 75mm (-25mm step): level decreases to 4 (3rd click)
        assert_eq!(session.process_distance(75), Some(4));
        assert_eq!(session.anchor_mm, 75);
        assert_eq!(session.current_level, 4);

        // Raise hand to 100mm (+25mm step): level increases back to 5
        assert_eq!(session.process_distance(100), Some(5));
        assert_eq!(session.anchor_mm, 100);
        assert_eq!(session.current_level, 5);

        // Hand removed (> 330mm): exits gesture session
        assert_eq!(session.process_distance(360), None);
        assert!(!session.in_gesture);
        assert_eq!(session.anchor_mm, 0);
    }

    #[test]
    fn test_relative_gesture_session_clamps_at_boundaries() {
        // Test ceiling clamp at 10
        let mut session = GestureSession::new(8);
        assert_eq!(session.process_distance(100), None);

        assert_eq!(session.process_distance(125), Some(9));
        assert_eq!(session.process_distance(150), Some(10));

        // Moving higher beyond 10 returns None (no more clicks!)
        assert_eq!(session.process_distance(175), None);
        assert_eq!(session.process_distance(200), None);
        assert_eq!(session.current_level, 10);

        // Test floor clamp at 1
        let mut session_floor = GestureSession::new(2);
        assert_eq!(session_floor.process_distance(150), None);

        assert_eq!(session_floor.process_distance(125), Some(1));

        // Moving lower beyond 1 returns None (no more clicks!)
        assert_eq!(session_floor.process_distance(100), None);
        assert_eq!(session_floor.process_distance(75), None);
        assert_eq!(session_floor.current_level, 1);
    }
}
