//! Alarm audio playback runner with volume ramping and timeout protection.

use crate::alarms::sound::{apply_volume_ramp, generate_synthetic_chime, load_wav_pcm_16};
use crate::audio::AudioPlayback;
pub use crate::audio::ALARM_RESPONSE_ID;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, warn};

const SAMPLE_RATE: u32 = 24_000;
const CHUNK_DURATION: Duration = Duration::from_millis(100);
const CHUNK_SAMPLES: usize = SAMPLE_RATE as usize / 10;
const MELODY_LOAD_TIMEOUT: Duration = Duration::from_millis(250);
// The native renderer accepts at most 300 seconds of PCM per response.
const MAX_DURATION: Duration = Duration::from_secs(300);

struct AlarmJob {
    epoch: u64,
    handle: JoinHandle<()>,
}

pub struct AlarmPlayer {
    playback: Arc<AudioPlayback>,
    job: Arc<Mutex<Option<AlarmJob>>>,
    melody_loader: Arc<Semaphore>,
}

impl AlarmPlayer {
    pub fn new(playback: Arc<AudioPlayback>) -> Self {
        Self {
            playback,
            job: Arc::new(Mutex::new(None)),
            melody_loader: Arc::new(Semaphore::new(1)),
        }
    }

    /// Check whether this player has a current alarm session.
    pub fn is_ringing(&self) -> bool {
        self.job.lock().unwrap().is_some()
    }

    /// Claim the renderer for an alarm and return the cancellation epoch of
    /// that specific ring. A subsequent start supersedes this epoch.
    pub fn start(
        &self,
        melody_path: Option<PathBuf>,
        max_duration: Duration,
        ramp_duration: Duration,
    ) -> u64 {
        let mut slot = self.job.lock().unwrap();
        if let Some(previous) = slot.take() {
            previous.handle.abort();
        }
        let epoch = self.playback.claim_alarm();
        let deadline = Instant::now() + max_duration.min(MAX_DURATION);
        let job = self.job.clone();
        let playback = self.playback.clone();
        let melody_loader = self.melody_loader.clone();
        let handle = tokio::spawn(async move {
            // The wall timeout includes melody loading and any wait for queue space.
            let _ = tokio::time::timeout_at(deadline, async {
                let samples = load_melody(melody_path, melody_loader).await;
                let mut looped = samples.iter().copied().cycle();
                let started = Instant::now();
                let mut chunks = tokio::time::interval_at(started, CHUNK_DURATION);
                chunks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                while playback.cancellation_epoch() == epoch {
                    chunks.tick().await;
                    let chunk: Vec<i16> = looped.by_ref().take(CHUNK_SAMPLES).collect();
                    let pcm_bytes = apply_volume_ramp(
                        &chunk,
                        started.elapsed().as_secs_f32(),
                        ramp_duration.as_secs_f32(),
                        0.15,
                        0.85,
                    )
                    .into_iter()
                    .flat_map(i16::to_le_bytes)
                    .collect();
                    if let Err(err) = playback.play(epoch, ALARM_RESPONSE_ID, pcm_bytes).await {
                        debug!("Alarm playback stopped: {err}");
                        break;
                    }
                }
            })
            .await;

            let mut slot = job.lock().unwrap();
            if slot.as_ref().is_some_and(|active| active.epoch == epoch) {
                playback.release_alarm(epoch);
                slot.take();
            }
        });
        *slot = Some(AlarmJob { epoch, handle });
        epoch
    }

    /// Stop the current alarm, if present.
    pub fn stop(&self) {
        self.stop_matching(None);
    }

    /// Stop only the ring identified by this epoch. Returns false if it has
    /// already completed or a newer ring has taken its place.
    pub fn stop_if_epoch(&self, epoch: u64) -> bool {
        self.stop_matching(Some(epoch))
    }

    fn stop_matching(&self, epoch: Option<u64>) -> bool {
        let mut slot = self.job.lock().unwrap();
        if epoch.is_some() && slot.as_ref().map(|active| active.epoch) != epoch {
            return false;
        }
        let Some(active) = slot.take() else {
            return false;
        };
        active.handle.abort();
        self.playback.release_alarm(active.epoch);
        true
    }
}

async fn load_melody(path: Option<PathBuf>, loader: Arc<Semaphore>) -> Vec<i16> {
    if let Some(path) = path {
        // A blocking read already in progress can outlive cancellation. Keep
        // only one such load per player and use the chime for overlapping starts.
        if let Ok(slot) = loader.try_acquire_owned() {
            let load = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                load_wav_pcm_16(&path)
            });
            if let Ok(Ok(Ok(samples))) = tokio::time::timeout(MELODY_LOAD_TIMEOUT, load).await {
                return samples;
            }
        }
        warn!("Alarm melody unavailable; using built-in chime");
    }
    generate_synthetic_chime(SAMPLE_RATE)
}

impl Drop for AlarmPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::PlaybackBackend;

    #[tokio::test]
    async fn test_alarm_player_start_and_stop() {
        let playback = Arc::new(
            AudioPlayback::start_with_backend("default".to_string(), 24000, PlaybackBackend::Mock)
                .expect("Failed to start mock playback"),
        );

        let player = AlarmPlayer::new(playback.clone());
        assert!(!player.is_ringing());

        player.start(None, Duration::from_secs(2), Duration::from_secs(1));

        assert!(player.is_ringing());

        tokio::time::timeout(Duration::from_secs(1), async {
            while !playback.is_playing() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Alarm should send PCM");

        player.stop();
        assert!(!player.is_ringing());
        assert!(!playback.is_playing());
    }

    #[tokio::test]
    async fn test_alarm_player_auto_timeout() {
        let playback = Arc::new(
            AudioPlayback::start_with_backend("default".to_string(), 24000, PlaybackBackend::Mock)
                .expect("Failed to start mock playback"),
        );

        let player = AlarmPlayer::new(playback.clone());
        player.start(
            None,
            Duration::from_millis(200), // Short timeout
            Duration::from_millis(50),
        );

        assert!(player.is_ringing());

        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(
            !player.is_ringing(),
            "Player should auto-stop after timeout"
        );
    }

    #[tokio::test]
    async fn idle_stop_and_drop_preserve_dialogue_playback() {
        let playback = Arc::new(
            AudioPlayback::start_with_backend("default".to_string(), 24_000, PlaybackBackend::Mock)
                .unwrap(),
        );
        let player = AlarmPlayer::new(playback.clone());
        let voice_epoch = playback.cancellation_epoch();
        playback.play(voice_epoch, 1, vec![0; 48]).await.unwrap();
        player.stop();
        drop(player);
        assert_eq!(playback.cancellation_epoch(), voice_epoch);
        assert!(playback.is_playing());
    }

    #[tokio::test]
    async fn superseded_alarm_cannot_stop_new_ring() {
        let playback = Arc::new(
            AudioPlayback::start_with_backend("default".to_string(), 24_000, PlaybackBackend::Mock)
                .unwrap(),
        );
        let player = AlarmPlayer::new(playback.clone());
        let old = player.start(None, Duration::from_millis(20), Duration::ZERO);
        let current = player.start(None, Duration::from_secs(2), Duration::ZERO);
        assert_ne!(old, current);
        assert!(!player.stop_if_epoch(old));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(player.is_ringing());
        assert_eq!(playback.cancellation_epoch(), current);
        assert!(player.stop_if_epoch(current));
        assert!(!player.is_ringing());
    }
}
