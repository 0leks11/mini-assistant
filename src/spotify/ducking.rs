//! Acoustic Music Ducking Manager for GPT Mini.
//!
//! Temporarily ducks music playback proportionally during voice interactions
//! so the microphone clearly captures the user's speech, then restores the original
//! volume strictly to its pre-duck value once the dialogue turn ends (or cancels
//! restoration if the user explicitly changed volume or paused).

use crate::audio::volume;
use crate::spotify::{PlaybackState, SoloistClient};
use std::sync::atomic::{AtomicU8, Ordering};
use tracing::info;

/// Calculate proportional ducked volume level (1..=10 scale):
/// V_duck = max(1, floor(V_orig * 0.25))
///
/// Returns None if volume is already <= 1 (no ducking needed).
pub fn calculate_ducked_level(orig_level: u8) -> Option<u8> {
    if orig_level <= 1 {
        return None;
    }
    let ducked = ((orig_level as f32) * 0.25).floor() as u8;
    Some(ducked.max(1))
}

#[derive(Debug, Default)]
pub struct MusicDucker {
    /// 0 = not currently ducked. 1..=10 = pre-duck master volume level.
    ducked_from_level: AtomicU8,
}

impl MusicDucker {
    pub fn new() -> Self {
        Self {
            ducked_from_level: AtomicU8::new(0),
        }
    }

    /// True if volume is currently ducked for voice interaction.
    #[allow(dead_code)]
    pub fn is_ducked(&self) -> bool {
        self.ducked_from_level.load(Ordering::Relaxed) > 0
    }

    /// Pre-duck level if currently ducked, or 0.
    #[allow(dead_code)]
    pub fn ducked_level(&self) -> u8 {
        self.ducked_from_level.load(Ordering::Relaxed)
    }

    /// Proportional acoustic ducking: query Soloist status and attenuate volume proportionally.
    pub async fn duck(&self, soloist: &SoloistClient) {
        // Query status from Soloist to check playback state and volume
        let status = soloist.get_status().await;
        let is_playing = match &status {
            Ok(s) => s.state == PlaybackState::Playing,
            Err(_) => false,
        };

        // If not playing, ducking is unnecessary
        if !is_playing {
            return;
        }

        // Determine current volume: prefer Soloist reported volume if non-zero, fallback to master level
        let current_lvl = match &status {
            Ok(s) if s.volume > 0 => ((s.volume as u16 + 5) / 10).clamp(1, 10) as u8,
            _ => volume::current_level(),
        };

        let Some(ducked_lvl) = calculate_ducked_level(current_lvl) else {
            info!(
                current_lvl,
                "Music volume is already at minimum; ducking skipped"
            );
            return;
        };

        if self
            .ducked_from_level
            .compare_exchange(0, current_lvl, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            info!(
                orig_level = current_lvl,
                ducked_level = ducked_lvl,
                ducked_pct = ducked_lvl * 10,
                "Acoustic barge-in: proportionally ducking music volume"
            );
            let _ = soloist.set_volume(ducked_lvl * 10).await;
        }
    }

    /// Restore volume strictly to pre-duck level if still ducked.
    pub async fn restore_if_ducked(&self, soloist: &SoloistClient) {
        let pre_level = self.ducked_from_level.swap(0, Ordering::SeqCst);
        if pre_level > 0 {
            let master_lvl = volume::current_level();
            let ducked_lvl = calculate_ducked_level(pre_level).unwrap_or(1);
            // If the user changed the master volume during the turn (not ducked and not pre_level),
            // respect the user's explicit choice. Otherwise restore pre-duck volume.
            let target_level = if master_lvl != ducked_lvl && master_lvl != pre_level {
                master_lvl
            } else {
                pre_level
            };
            info!(
                restored_level = target_level,
                orig_level = pre_level,
                "Dialogue ended: restoring music volume strictly to pre-duck level"
            );
            let _ = soloist.set_volume(target_level * 10).await;
            volume::set_volume_level(target_level);
        }
    }

    /// Override pre-duck level or cancel ducking when user explicitly sets volume.
    #[allow(dead_code)]
    pub fn override_target(&self, _new_level: u8) {
        self.cancel();
    }

    /// Cancel ducking state without restoring (e.g. if playback was stopped or user explicitly set volume).
    pub fn cancel(&self) {
        self.ducked_from_level.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_ducked_level_formula() {
        // Levels 0 and 1: no ducking needed
        assert_eq!(calculate_ducked_level(0), None);
        assert_eq!(calculate_ducked_level(1), None);

        // Level 2 -> floor(2 * 0.25) = 0 -> max(1, 0) = 1
        assert_eq!(calculate_ducked_level(2), Some(1));
        // Level 3 -> floor(3 * 0.25) = 0 -> max(1, 0) = 1
        assert_eq!(calculate_ducked_level(3), Some(1));
        // Level 4 -> floor(4 * 0.25) = 1 -> max(1, 1) = 1
        assert_eq!(calculate_ducked_level(4), Some(1));
        // Level 5 -> floor(5 * 0.25) = 1 -> max(1, 1) = 1
        assert_eq!(calculate_ducked_level(5), Some(1));
        // Level 6 -> floor(6 * 0.25) = 1 -> max(1, 1) = 1
        assert_eq!(calculate_ducked_level(6), Some(1));
        // Level 7 -> floor(7 * 0.25) = 1 -> max(1, 1) = 1
        assert_eq!(calculate_ducked_level(7), Some(1));

        // Level 8 -> floor(8 * 0.25) = 2 -> max(1, 2) = 2
        assert_eq!(calculate_ducked_level(8), Some(2));
        // Level 9 -> floor(9 * 0.25) = 2 -> max(1, 2) = 2
        assert_eq!(calculate_ducked_level(9), Some(2));
        // Level 10 -> floor(10 * 0.25) = 2 -> max(1, 2) = 2
        assert_eq!(calculate_ducked_level(10), Some(2));
    }

    #[test]
    fn test_music_ducker_lifecycle() {
        let ducker = MusicDucker::new();
        assert!(!ducker.is_ducked());

        volume::set_volume_level(8);
        assert_eq!(volume::current_level(), 8);

        // Simulate ducking state at level 8
        let current = volume::current_level();
        let ducked = calculate_ducked_level(current).unwrap();
        assert_eq!(ducked, 2);

        ducker.ducked_from_level.store(current, Ordering::SeqCst);
        assert!(ducker.is_ducked());
        assert_eq!(ducker.ducked_level(), 8);

        // Cancel
        ducker.cancel();
        assert!(!ducker.is_ducked());

        // Restore to 10
        volume::set_volume_level(10);
    }

    #[test]
    fn test_music_ducker_low_volume_preservation() {
        let ducker = MusicDucker::new();

        // When listening at quiet volume 2:
        volume::set_volume_level(2);
        let ducked = calculate_ducked_level(2).unwrap();
        assert_eq!(ducked, 1);

        ducker.ducked_from_level.store(2, Ordering::SeqCst);
        assert_eq!(ducker.ducked_level(), 2);

        // User didn't touch volume, so restoring restores strictly to 2 (NOT 10)
        let pre_level = ducker.ducked_from_level.swap(0, Ordering::SeqCst);
        assert_eq!(pre_level, 2);

        // Reset
        volume::set_volume_level(10);
    }

    #[test]
    fn test_user_override_cancels_ducking() {
        let ducker = MusicDucker::new();
        volume::set_volume_level(7);
        ducker.ducked_from_level.store(7, Ordering::SeqCst);
        assert!(ducker.is_ducked());

        // User explicitly set new volume
        ducker.override_target(5);
        assert!(!ducker.is_ducked());
        assert_eq!(ducker.ducked_level(), 0);

        // Reset
        volume::set_volume_level(10);
    }
}
