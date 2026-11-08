//! Passive hardware button detector for Station Mini.
//!
//! Specifically monitors `adc_keypad` for `KEY_MUTE` (code 113):
//! - Passive monitoring without `EVIOCGRAB` (vendor and ALSA handlers remain uninterrupted).
//! - Short press (< 5.0s): emits `MuteButtonEvent::ShortPress`.
//! - Long hold (>= 5.0s): emits `MuteButtonEvent::LongPress` (e.g. to enter BLE setup).
//! - Ignores kernel auto-repeats (`value == 2`).
//! - Discards buttons already held at startup (avoids false triggers on initial release).
//! - Tested deterministically with synthetic events and mock clock.

use crate::alarms::clock::Clock;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Target Linux input event constants
pub const EV_KEY: u16 = 1;
pub const KEY_MUTE: u16 = 113;
pub const KEY_VALUE_RELEASE: i32 = 0;
pub const KEY_VALUE_PRESS: i32 = 1;
pub const KEY_VALUE_REPEAT: i32 = 2;

/// Threshold duration required to consider a hold a long press
pub const LONG_PRESS_THRESHOLD: Duration = Duration::from_secs(5);

/// Native Linux input event
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawInputEvent {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

/// Parse Linux input_event supporting both 16-byte (32-bit Linux ABI)
/// and 24-byte (64-bit Linux ABI) framing.
pub fn parse_input_event(buf: &[u8]) -> Option<RawInputEvent> {
    if buf.len() == 16 {
        // 32-bit Linux input_event:
        // [0..4]: tv_sec (i32)
        // [4..8]: tv_usec (i32)
        // [8..10]: type (u16)
        // [10..12]: code (u16)
        // [12..16]: value (i32)
        let type_ = u16::from_le_bytes([buf[8], buf[9]]);
        let code = u16::from_le_bytes([buf[10], buf[11]]);
        let value = i32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Some(RawInputEvent { type_, code, value })
    } else if buf.len() == 24 {
        // 64-bit Linux input_event:
        // [0..8]: tv_sec (i64)
        // [8..16]: tv_usec (i64)
        // [16..18]: type (u16)
        // [18..20]: code (u16)
        // [20..24]: value (i32)
        let type_ = u16::from_le_bytes([buf[16], buf[17]]);
        let code = u16::from_le_bytes([buf[18], buf[19]]);
        let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
        Some(RawInputEvent { type_, code, value })
    } else {
        None
    }
}

/// Serialized event frame size for target architecture
pub const fn native_input_event_size() -> usize {
    if std::mem::size_of::<usize>() == 4 {
        16
    } else {
        24
    }
}

/// Detected button event
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteButtonEvent {
    /// Button pressed and released in under 5.0 seconds
    ShortPress { duration: Duration },
    /// Button held continuously for 5.0 seconds or longer
    LongPress { duration: Duration },
}

/// Pure state machine for detecting MUTE button holds
pub struct MuteHoldDetector<C: Clock> {
    clock: C,
    pressed_at: Option<std::time::Instant>,
    threshold: Duration,
}

impl<C: Clock> MuteHoldDetector<C> {
    pub fn new(clock: C) -> Self {
        Self::with_threshold(clock, LONG_PRESS_THRESHOLD)
    }

    pub fn with_threshold(clock: C, threshold: Duration) -> Self {
        Self {
            clock,
            pressed_at: None,
            threshold,
        }
    }

    /// Process a parsed input event.
    ///
    /// Returns `Some(MuteButtonEvent)` exactly once on confirmed release (`value == 0`).
    pub fn handle_event(&mut self, event: &RawInputEvent) -> Option<MuteButtonEvent> {
        // We only track EV_KEY on KEY_MUTE
        if event.type_ != EV_KEY || event.code != KEY_MUTE {
            return None;
        }

        match event.value {
            KEY_VALUE_PRESS => {
                // If already pressed, ignore duplicate press event
                if self.pressed_at.is_none() {
                    self.pressed_at = Some(self.clock.now_monotonic());
                }
                None
            }

            KEY_VALUE_REPEAT => {
                // Kernel auto-repeat while holding: keep existing start time intact
                None
            }

            KEY_VALUE_RELEASE => {
                let Some(start) = self.pressed_at.take() else {
                    // Button was already pressed before watcher started, or spurious release:
                    // Discard unmeasured hold to avoid false triggers
                    return None;
                };

                let duration = self.clock.now_monotonic().duration_since(start);
                if duration >= self.threshold {
                    Some(MuteButtonEvent::LongPress { duration })
                } else {
                    Some(MuteButtonEvent::ShortPress { duration })
                }
            }

            _ => None,
        }
    }

    pub fn is_pressed(&self) -> bool {
        self.pressed_at.is_some()
    }

    pub fn reset(&mut self) {
        self.pressed_at = None;
    }
}

/// Locate the `adc_keypad` event node by parsing `/proc/bus/input/devices`
pub fn parse_input_devices_for_keypad(content: &str) -> Option<PathBuf> {
    let mut in_adc_keypad = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("N: Name=") {
            in_adc_keypad = trimmed.contains("adc_keypad");
        } else if in_adc_keypad && trimmed.starts_with("H: Handlers=") {
            for token in trimmed["H: Handlers=".len()..].split_whitespace() {
                if token.starts_with("event") {
                    return Some(PathBuf::from("/dev/input").join(token));
                }
            }
        }
    }
    None
}

/// Find the adc_keypad input device, falling back to `/dev/input/event2`
pub async fn find_adc_keypad_device() -> PathBuf {
    if let Ok(content) = tokio::fs::read_to_string("/proc/bus/input/devices").await {
        if let Some(path) = parse_input_devices_for_keypad(&content) {
            if tokio::fs::metadata(&path).await.is_ok() {
                return path;
            }
        }
    }

    PathBuf::from("/dev/input/event2")
}

/// Asynchronous runner that passively watches `/dev/input/event*` for MUTE events.
/// Runs without `EVIOCGRAB` so the existing system and ALSA mute behaviors remain unaffected.
pub async fn run_mute_button_watcher<C: Clock>(
    device_path: &Path,
    clock: C,
    tx_events: tokio::sync::mpsc::Sender<MuteButtonEvent>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;

    info!(
        path = %device_path.display(),
        "Opening mute button device for passive monitoring (no EVIOCGRAB)"
    );

    let mut file = tokio::fs::File::open(device_path).await?;
    let mut detector = MuteHoldDetector::new(clock);

    let event_size = native_input_event_size();
    let mut buf = vec![0u8; event_size * 16];
    let mut pending = Vec::new();

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            warn!("Mute button device reached EOF");
            break;
        }

        pending.extend_from_slice(&buf[..n]);

        while pending.len() >= event_size {
            let chunk: Vec<u8> = pending.drain(..event_size).collect();
            if let Some(event) = parse_input_event(&chunk) {
                if let Some(button_event) = detector.handle_event(&event) {
                    match button_event {
                        MuteButtonEvent::ShortPress { duration } => {
                            debug!(elapsed_ms = duration.as_millis(), "Mute button short press");
                        }
                        MuteButtonEvent::LongPress { duration } => {
                            info!(
                                elapsed_ms = duration.as_millis(),
                                "Mute button 5-second long press detected (BLE setup candidate)"
                            );
                        }
                    }

                    if tx_events.send(button_event).await.is_err() {
                        debug!("Button event receiver closed, terminating watcher");
                        return Ok(());
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
    use crate::alarms::clock::TestClock;
    use time::macros::datetime;

    fn make_test_detector() -> (MuteHoldDetector<TestClock>, TestClock) {
        let clock = TestClock::new(datetime!(2026-11-07 00:00:00 UTC));
        let detector = MuteHoldDetector::new(clock.clone());
        (detector, clock)
    }

    #[test]
    fn test_short_press_under_five_seconds_emits_short_press() {
        let (mut detector, clock) = make_test_detector();

        // 1. Press
        let ev_press = RawInputEvent {
            type_: EV_KEY,
            code: KEY_MUTE,
            value: KEY_VALUE_PRESS,
        };
        assert_eq!(detector.handle_event(&ev_press), None);
        assert!(detector.is_pressed());

        // 2. Advance clock by 4.9 seconds (strictly below 5.0s threshold)
        clock.advance(Duration::from_millis(4900));

        // 3. Release
        let ev_release = RawInputEvent {
            type_: EV_KEY,
            code: KEY_MUTE,
            value: KEY_VALUE_RELEASE,
        };
        let result = detector.handle_event(&ev_release);

        assert_eq!(
            result,
            Some(MuteButtonEvent::ShortPress {
                duration: Duration::from_millis(4900)
            })
        );
        assert!(!detector.is_pressed());
    }

    #[test]
    fn test_long_press_exactly_five_seconds_emits_long_press() {
        let (mut detector, clock) = make_test_detector();

        // 1. Press
        detector.handle_event(&RawInputEvent {
            type_: EV_KEY,
            code: KEY_MUTE,
            value: KEY_VALUE_PRESS,
        });

        // 2. Advance clock by exactly 5.0 seconds
        clock.advance(Duration::from_millis(5000));

        // 3. Release
        let result = detector.handle_event(&RawInputEvent {
            type_: EV_KEY,
            code: KEY_MUTE,
            value: KEY_VALUE_RELEASE,
        });

        assert_eq!(
            result,
            Some(MuteButtonEvent::LongPress {
                duration: Duration::from_millis(5000)
            })
        );
        assert!(!detector.is_pressed());
    }

    #[test]
    fn test_long_press_with_auto_repeat_events_does_not_reset_or_duplicate() {
        let (mut detector, clock) = make_test_detector();

        // 1. Press at T = 0
        detector.handle_event(&RawInputEvent {
            type_: EV_KEY,
            code: KEY_MUTE,
            value: KEY_VALUE_PRESS,
        });

        // 2. Kernel auto-repeats every 250ms
        for _ in 0..20 {
            clock.advance(Duration::from_millis(250));
            let rep = detector.handle_event(&RawInputEvent {
                type_: EV_KEY,
                code: KEY_MUTE,
                value: KEY_VALUE_REPEAT,
            });
            assert_eq!(rep, None, "Repeat must not emit any event");
        }

        // Total elapsed: 20 * 250ms = 5000ms
        // 3. Release
        let result = detector.handle_event(&RawInputEvent {
            type_: EV_KEY,
            code: KEY_MUTE,
            value: KEY_VALUE_RELEASE,
        });

        assert_eq!(
            result,
            Some(MuteButtonEvent::LongPress {
                duration: Duration::from_millis(5000)
            })
        );
    }

    #[test]
    fn test_button_already_held_at_startup_discards_initial_release() {
        let (mut detector, clock) = make_test_detector();

        // Process starts while button is already pressed.
        // First event seen by the detector is release:
        clock.advance(Duration::from_secs(10));
        let result = detector.handle_event(&RawInputEvent {
            type_: EV_KEY,
            code: KEY_MUTE,
            value: KEY_VALUE_RELEASE,
        });

        // Must discard unmeasured hold!
        assert_eq!(
            result, None,
            "Initial release without press must be discarded"
        );
        assert!(!detector.is_pressed());
    }

    #[test]
    fn test_non_mute_keys_are_completely_ignored() {
        let (mut detector, _clock) = make_test_detector();

        // Volume Up (115), Volume Down (114), Power (116)
        for code in [114, 115, 116] {
            assert_eq!(
                detector.handle_event(&RawInputEvent {
                    type_: EV_KEY,
                    code,
                    value: KEY_VALUE_PRESS,
                }),
                None
            );
            assert_eq!(
                detector.handle_event(&RawInputEvent {
                    type_: EV_KEY,
                    code,
                    value: KEY_VALUE_RELEASE,
                }),
                None
            );
            assert!(!detector.is_pressed());
        }
    }

    #[test]
    fn test_parse_input_devices_for_adc_keypad() {
        let sample = r#"
I: Bus=0000 Vendor=0000 Product=0000 Version=0000
N: Name="STM VL53L0 proximity sensor"
H: Handlers=event0 

I: Bus=0000 Vendor=0000 Product=0000 Version=0000
N: Name="AML-AUGESOUND Headphones"
H: Handlers=event1 

I: Bus=0010 Vendor=0001 Product=0001 Version=0100
N: Name="adc_keypad"
P: Phys=adc_keypad/input0
S: Sysfs=/devices/platform/adc_keypad/input/input2
U: Uniq=
H: Handlers=kbd event2 
B: KEY=1e0000 0 0 0
"#;

        let path = parse_input_devices_for_keypad(sample);
        assert_eq!(path, Some(PathBuf::from("/dev/input/event2")));
    }

    #[test]
    fn test_parse_binary_input_event_32bit_and_64bit() {
        // 16-byte buffer (32-bit Linux)
        let mut buf32 = [0u8; 16];
        // tv_sec = 100
        buf32[0..4].copy_from_slice(&100i32.to_le_bytes());
        // tv_usec = 200
        buf32[4..8].copy_from_slice(&200i32.to_le_bytes());
        // type = EV_KEY (1)
        buf32[8..10].copy_from_slice(&EV_KEY.to_le_bytes());
        // code = KEY_MUTE (113)
        buf32[10..12].copy_from_slice(&KEY_MUTE.to_le_bytes());
        // value = 1 (press)
        buf32[12..16].copy_from_slice(&KEY_VALUE_PRESS.to_le_bytes());

        let ev32 = parse_input_event(&buf32).expect("32-bit event parsed");
        assert_eq!(ev32.type_, EV_KEY);
        assert_eq!(ev32.code, KEY_MUTE);
        assert_eq!(ev32.value, KEY_VALUE_PRESS);

        // 24-byte buffer (64-bit Linux)
        let mut buf64 = [0u8; 24];
        buf64[0..8].copy_from_slice(&100i64.to_le_bytes());
        buf64[8..16].copy_from_slice(&200i64.to_le_bytes());
        buf64[16..18].copy_from_slice(&EV_KEY.to_le_bytes());
        buf64[18..20].copy_from_slice(&KEY_MUTE.to_le_bytes());
        buf64[20..24].copy_from_slice(&KEY_VALUE_RELEASE.to_le_bytes());

        let ev64 = parse_input_event(&buf64).expect("64-bit event parsed");
        assert_eq!(ev64.type_, EV_KEY);
        assert_eq!(ev64.code, KEY_MUTE);
        assert_eq!(ev64.value, KEY_VALUE_RELEASE);
    }
}
