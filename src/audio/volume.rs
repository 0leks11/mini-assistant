//! Software volume control for GPT Mini.
//!
//! Maintains volume level on a 0–10 scale (0 = mute, 10 = max).
//! Controls:
//! 1. Assistant voice and alarm PCM audio via direct sample scaling in `AudioPlayback`.
//! 2. Spotify Soloist playback via `soloist.set_volume(level * 10)`.
//! 3. Proximity sensor (VL53L0) hand-distance gestures.

use std::sync::atomic::{AtomicU8, Ordering};
use tracing::info;

/// Current volume level (0–10), shared across audio playback, voice tools, and VL53 sensor.
/// Default is 10 (100% normal playback gain).
static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(10);

/// Get current volume level (0–10).
pub fn current_level() -> u8 {
    CURRENT_LEVEL.load(Ordering::Relaxed)
}

/// Set volume level (0–10). Clamps to 10. Returns the newly set level.
pub fn set_volume_level(level: u8) -> u8 {
    let level = level.min(10);
    CURRENT_LEVEL.store(level, Ordering::Relaxed);
    info!(level, "Assistant master volume level set (0-10)");
    level
}

/// Increase volume by 1 step (max 10). Returns new level.
pub fn volume_up() -> u8 {
    let cur = current_level();
    let new = (cur + 1).min(10);
    set_volume_level(new)
}

/// Decrease volume by 1 step (min 0). Returns new level.
pub fn volume_down() -> u8 {
    let cur = current_level();
    let new = cur.saturating_sub(1);
    set_volume_level(new)
}

/// Set volume from percent (0–100). Maps 0-100 to 0-10.
pub fn set_volume_pct(pct: u8) -> u8 {
    let level = ((pct as u16 + 5) / 10).min(10) as u8;
    set_volume_level(level)
}

/// Apply volume scaling to a 16-bit PCM sample according to the current volume level.
#[inline]
pub fn scale_sample(sample: i16, level: u8) -> i16 {
    let lvl = level.min(10) as i32;
    ((sample as i32 * lvl) / 10) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_scale_bounds() {
        set_volume_level(10);
        assert_eq!(current_level(), 10);
        assert_eq!(scale_sample(1000, 10), 1000);
        assert_eq!(scale_sample(-1000, 10), -1000);

        set_volume_level(5);
        assert_eq!(current_level(), 5);
        assert_eq!(scale_sample(1000, 5), 500);
        assert_eq!(scale_sample(-1000, 5), -500);

        set_volume_level(0);
        assert_eq!(current_level(), 0);
        assert_eq!(scale_sample(1000, 0), 0);

        // Reset to 10 for following tests
        set_volume_level(10);
    }

    #[test]
    fn test_volume_up_down() {
        set_volume_level(5);
        assert_eq!(volume_up(), 6);
        assert_eq!(volume_down(), 5);
        assert_eq!(volume_down(), 4);

        set_volume_level(10);
        assert_eq!(volume_up(), 10); // clamped

        set_volume_level(0);
        assert_eq!(volume_down(), 0); // clamped

        // Reset to 10
        set_volume_level(10);
    }

    #[test]
    fn test_set_volume_pct() {
        assert_eq!(set_volume_pct(0), 0);
        assert_eq!(set_volume_pct(48), 5);
        assert_eq!(set_volume_pct(52), 5);
        assert_eq!(set_volume_pct(100), 10);

        // Reset to 10
        set_volume_level(10);
    }
}
