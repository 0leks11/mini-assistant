//! Software volume control for GPT Mini.
//!
//! Maintains volume level on a 0–10 scale (0 = mute, 10 = max).
//! Controls:
//! 1. Assistant voice and alarm PCM audio via direct sample scaling in `AudioPlayback`.
//! 2. Spotify Soloist playback via `soloist.set_volume(level * 10)`.
//! 3. Proximity sensor (VL53L0) hand-distance gestures.

use std::sync::atomic::{AtomicU8, Ordering};
use tracing::info;

/// Safe default volume level on daemon startup/reboot (1 = 10% gain).
pub const DEFAULT_STARTUP_LEVEL: u8 = 1;

/// Current volume level (0–10), shared across audio playback, voice tools, and VL53 sensor.
/// Default is 1 (10% safe level on boot/restart).
static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(DEFAULT_STARTUP_LEVEL);

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

/// Generate the static 48000Hz stereo MPCM byte sequence for a crisp 20ms tactile click ("клац").
pub fn generate_click_mpcm() -> Vec<u8> {
    const SAMPLE_RATE: u32 = 48000;
    const DURATION_SECS: f32 = 0.020; // 20ms
    let num_samples = (SAMPLE_RATE as f32 * DURATION_SECS) as usize; // 960 frames
    let pcm_len = num_samples * 4; // 2 channels * 2 bytes = 3840 bytes

    let mut out = Vec::with_capacity(16 + 8 + pcm_len + 8);

    // 1. MPCM Preamble (16 bytes): "MPCM", ver=1, rate=48000, ch=2, bits=16
    out.extend_from_slice(b"MPCM");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&48000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());

    // 2. DATA Record Header (8 bytes): type=1, length=pcm_len
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(pcm_len as u32).to_le_bytes());

    // 3. PCM Data: 20ms 1800Hz exponential decay
    for i in 0..num_samples {
        let t = i as f32 / SAMPLE_RATE as f32;
        let decay = (-t * 250.0).exp();
        let wave = (2.0 * std::f32::consts::PI * 1800.0 * t).sin()
            + 0.3 * (2.0 * std::f32::consts::PI * 2700.0 * t).sin();
        let sample = (wave * decay * 14000.0).clamp(-16000.0, 16000.0) as i16;
        let le = sample.to_le_bytes();
        // Stereo: left and right
        out.extend_from_slice(&le);
        out.extend_from_slice(&le);
    }

    // 4. END Record Header (8 bytes): type=2, length=0
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    out
}

static CLICK_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<()>> =
    std::sync::OnceLock::new();

/// Trigger a non-blocking acoustic tactile click ("клац") through the audio sink.
pub fn trigger_volume_click() {
    if let Some(tx) = CLICK_TX.get() {
        let _ = tx.send(());
    }
}

/// Initialize the acoustic click worker task.
pub fn init_volume_clicker() {
    if CLICK_TX.get().is_some() {
        return;
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let _ = CLICK_TX.set(tx);

    let click_mpcm = std::sync::Arc::new(generate_click_mpcm());

    std::thread::Builder::new()
        .name("volume-clicker".into())
        .spawn(move || {
            let sink_path = std::path::Path::new("/data/opt/mini-assistant/mini_pcm_sink");
            while let Some(()) = rx.blocking_recv() {
                #[cfg(target_os = "linux")]
                if sink_path.exists() {
                    use std::io::Write;
                    if let Ok(mut child) = std::process::Command::new(sink_path)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(&click_mpcm);
                            let _ = stdin.flush();
                        }
                        let _ = child.wait();
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = &click_mpcm;
                    let _ = &sink_path;
                }
            }
        })
        .expect("Failed to spawn volume clicker thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_click_mpcm_structure() {
        let mpcm = generate_click_mpcm();
        assert_eq!(&mpcm[0..4], b"MPCM");
        assert_eq!(u32::from_le_bytes(mpcm[4..8].try_into().unwrap()), 1); // ver
        assert_eq!(u32::from_le_bytes(mpcm[8..12].try_into().unwrap()), 48000); // rate
        assert_eq!(u16::from_le_bytes(mpcm[12..14].try_into().unwrap()), 2); // ch
        assert_eq!(u16::from_le_bytes(mpcm[14..16].try_into().unwrap()), 16); // bits

        // Total length = 16 (preamble) + 8 (data hdr) + 3840 (pcm) + 8 (end hdr) = 3872
        assert_eq!(mpcm.len(), 3872);

        // Verify end record header at end: type=2, len=0
        assert_eq!(u32::from_le_bytes(mpcm[3864..3868].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(mpcm[3868..3872].try_into().unwrap()), 0);
    }

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
