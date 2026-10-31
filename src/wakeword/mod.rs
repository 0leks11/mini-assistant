use anyhow::Result;
use tracing::debug;

pub trait WakeWordEngine: Send + Sync {
    /// Process incoming 16-bit 16kHz PCM audio frame (e.g. 512 bytes / 256 samples).
    /// Returns true if wake word was detected in this frame.
    fn process(&mut self, pcm_chunk: &[u8]) -> bool;

    /// Reset internal state
    fn reset(&mut self);
}

/// Simple energy-based Voice Activity Detector (VAD) fallback
pub struct SimpleVadEngine {
    threshold_rms: f32,
    consecutive_active: usize,
    required_consecutive: usize,
}

impl SimpleVadEngine {
    pub fn new(threshold_rms: f32) -> Self {
        Self {
            threshold_rms,
            consecutive_active: 0,
            required_consecutive: 3, // ~50ms of audio
        }
    }
}

impl WakeWordEngine for SimpleVadEngine {
    fn process(&mut self, pcm_chunk: &[u8]) -> bool {
        if pcm_chunk.len() < 2 {
            return false;
        }

        let mut sum_sq = 0.0f64;
        let mut sample_count = 0;

        for chunk in pcm_chunk.chunks_exact(2) {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f64;
            sum_sq += sample * sample;
            sample_count += 1;
        }

        let rms = ((sum_sq / sample_count as f64).sqrt()) as f32;

        if rms > self.threshold_rms {
            self.consecutive_active += 1;
            if self.consecutive_active >= self.required_consecutive {
                debug!("VAD voice detected (RMS: {:.1} > {:.1})", rms, self.threshold_rms);
                return true;
            }
        } else {
            self.consecutive_active = self.consecutive_active.saturating_sub(1);
        }

        false
    }

    fn reset(&mut self) {
        self.consecutive_active = 0;
    }
}

/// Dummy pass-through engine for continuous open-mic mode
pub struct AlwaysActiveEngine;

impl WakeWordEngine for AlwaysActiveEngine {
    fn process(&mut self, _pcm_chunk: &[u8]) -> bool {
        true
    }
    fn reset(&mut self) {}
}
