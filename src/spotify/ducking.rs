//! Acoustic Music Ducking Manager for GPT Mini.
//!
//! Temporarily ducks music playback (e.g. down to 20%) during voice interactions
//! so the microphone clearly captures the user's speech, then restores the original
//! volume once the dialogue turn ends (or cancels restoration if the user explicitly
//! changed volume or paused).

use crate::audio::volume;
use crate::spotify::SoloistClient;
use std::sync::atomic::{AtomicU8, Ordering};
use tracing::info;

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

    /// Duck Soloist volume down to 20% (level 2) and record the current level.
    pub async fn duck(&self, soloist: &SoloistClient) {
        let current_lvl = volume::current_level();
        // If current volume is already <= 2, no need to duck
        if current_lvl <= 2 {
            info!(current_lvl, "Music volume is already low; ducking skipped");
            return;
        }
        if self
            .ducked_from_level
            .compare_exchange(0, current_lvl, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            info!(
                from_level = current_lvl,
                to_level = 2,
                "Acoustic barge-in: ducking music volume to 20%"
            );
            let _ = soloist.set_volume(20).await;
        }
    }

    /// Restore volume to pre-duck level if still ducked.
    pub async fn restore_if_ducked(&self, soloist: &SoloistClient) {
        let pre_level = self.ducked_from_level.swap(0, Ordering::SeqCst);
        if pre_level > 0 {
            let master_lvl = volume::current_level();
            // If the user changed the master volume during the turn (not 2 and not pre_level),
            // respect the user's explicit choice. Otherwise restore pre-duck volume.
            let target_level = if master_lvl != 2 && master_lvl != pre_level {
                master_lvl
            } else {
                pre_level
            };
            info!(
                restored_level = target_level,
                "Dialogue ended: restoring music volume"
            );
            let _ = soloist.set_volume(target_level * 10).await;
        }
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
    fn test_music_ducker_lifecycle() {
        let ducker = MusicDucker::new();
        assert!(!ducker.is_ducked());

        volume::set_volume_level(8);
        assert_eq!(volume::current_level(), 8);

        // Simulate ducking state
        let current = volume::current_level();
        ducker.ducked_from_level.store(current, Ordering::SeqCst);
        assert!(ducker.is_ducked());

        // Cancel
        ducker.cancel();
        assert!(!ducker.is_ducked());

        // Restore to 10
        volume::set_volume_level(10);
    }
}
