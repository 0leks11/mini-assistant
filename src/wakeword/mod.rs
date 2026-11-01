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

        let (chunks, _) = pcm_chunk.as_chunks::<2>();
        for chunk in chunks {
            let sample = i16::from_le_bytes(*chunk) as f64;
            sum_sq += sample * sample;
            sample_count += 1;
        }

        let rms = ((sum_sq / sample_count as f64).sqrt()) as f32;

        if rms > self.threshold_rms {
            self.consecutive_active += 1;
            if self.consecutive_active >= self.required_consecutive {
                debug!(
                    "VAD voice detected (RMS: {:.1} > {:.1})",
                    rms, self.threshold_rms
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vad_empty_or_short_input() {
        let mut vad = SimpleVadEngine::new(500.0);
        assert!(!vad.process(&[]));
        assert!(!vad.process(&[0x00]));
    }

    #[test]
    fn test_vad_silence() {
        let mut vad = SimpleVadEngine::new(500.0);
        let silence = [0u8; 512];
        for _ in 0..10 {
            assert!(!vad.process(&silence));
        }
    }

    #[test]
    fn test_vad_consecutive_activation() {
        let mut vad = SimpleVadEngine::new(1000.0);

        // Frame with high energy amplitude (approx ~5000 RMS)
        let mut loud_frame = Vec::new();
        for i in 0..256 {
            let sample: i16 = if i % 2 == 0 { 5000 } else { -5000 };
            loud_frame.extend_from_slice(&sample.to_le_bytes());
        }

        // Frame 1: active count = 1, returns false
        assert!(!vad.process(&loud_frame));
        // Frame 2: active count = 2, returns false
        assert!(!vad.process(&loud_frame));
        // Frame 3: active count = 3 >= required_consecutive (3), returns true!
        assert!(vad.process(&loud_frame));

        // After reset, counter is zeroed
        vad.reset();
        assert!(!vad.process(&loud_frame));
    }

    #[test]
    fn test_always_active_engine() {
        let mut engine = AlwaysActiveEngine;
        assert!(engine.process(&[]));
        assert!(engine.process(&[1, 2, 3, 4]));
        engine.reset();
        assert!(engine.process(&[]));
    }
}
