pub mod enrollment;
pub mod keyword;
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
    is_speaking: bool,
}

impl SimpleVadEngine {
    pub fn new(threshold_rms: f32) -> Self {
        Self {
            threshold_rms,
            consecutive_active: 0,
            required_consecutive: 3, // ~50ms of audio for onset
            is_speaking: false,
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
            if self.is_speaking {
                return true;
            }
            self.consecutive_active += 1;
            if self.consecutive_active >= self.required_consecutive {
                self.is_speaking = true;
                debug!(
                    "VAD voice detected (RMS: {:.1} > {:.1})",
                    rms, self.threshold_rms
                );
                return true;
            }
        } else {
            self.consecutive_active = 0;
        }

        false
    }

    fn reset(&mut self) {
        self.consecutive_active = 0;
        self.is_speaking = false;
    }
}

/// Dummy pass-through engine for continuous open-mic mode
#[cfg(test)]
pub struct AlwaysActiveEngine;

#[cfg(test)]
impl WakeWordEngine for AlwaysActiveEngine {
    fn process(&mut self, _pcm_chunk: &[u8]) -> bool {
        true
    }
    fn reset(&mut self) {}
}

/// Manages the activation state of audio transmission to Gemini Live.
/// In gated mode, ambient microphone frames are kept in a circular pre-roll buffer
/// sized by audio duration (bytes/samples) and NEVER transmitted to the cloud until activation occurs.
pub struct InputGate {
    engine: Box<dyn WakeWordEngine>,
    speech_engine: Option<Box<dyn WakeWordEngine>>,
    manual_armed: bool,
    is_active: bool,
    has_spoken: bool,
    pre_roll_buffer: std::collections::VecDeque<Vec<u8>>,
    buffered_bytes: usize,
    max_pre_roll_bytes: usize,
    silence_bytes: usize,
    max_silence_bytes: usize,
    max_initial_silence_bytes: usize,
    continuous_mode: bool,
}

impl InputGate {
    /// Create a new InputGate configured by milliseconds and sample rate (16-bit mono PCM).
    pub fn new(
        engine: Box<dyn WakeWordEngine>,
        pre_roll_ms: u32,
        silence_timeout_ms: u32,
        sample_rate: u32,
        continuous_mode: bool,
    ) -> Self {
        let bytes_per_ms = (sample_rate as usize * 2) / 1000;
        let max_pre_roll_bytes = (pre_roll_ms as usize) * bytes_per_ms;
        let max_silence_bytes = (silence_timeout_ms as usize) * bytes_per_ms;

        Self {
            engine,
            speech_engine: None,
            manual_armed: false,
            is_active: continuous_mode,
            has_spoken: false,
            pre_roll_buffer: std::collections::VecDeque::new(),
            buffered_bytes: 0,
            max_pre_roll_bytes,
            silence_bytes: 0,
            max_silence_bytes,
            max_initial_silence_bytes: max_silence_bytes,
            continuous_mode,
        }
    }

    pub fn with_activation(
        activation: Box<dyn WakeWordEngine>,
        vad_threshold: f32,
        pre_roll_ms: u32,
        silence_ms: u32,
        rate: u32,
    ) -> Self {
        let mut gate = Self::new(activation, pre_roll_ms, silence_ms, rate, false);
        gate.speech_engine = Some(Box::new(SimpleVadEngine::new(vad_threshold)));
        gate
    }

    pub fn with_activation_and_initial_timeout(
        activation: Box<dyn WakeWordEngine>,
        vad_threshold: f32,
        pre_roll_ms: u32,
        initial_silence_ms: u32,
        silence_ms: u32,
        rate: u32,
    ) -> Self {
        let bytes_per_ms = (rate as usize * 2) / 1000;
        let mut gate =
            Self::with_activation(activation, vad_threshold, pre_roll_ms, silence_ms, rate);
        gate.max_initial_silence_bytes = (initial_silence_ms as usize) * bytes_per_ms;
        gate
    }

    pub fn set_manual_armed(&mut self, armed: bool) {
        self.manual_armed = armed;
    }

    /// Process an incoming PCM frame from the microphone.
    /// Returns audio chunks to forward to the cloud API, or empty Vec if gated.
    pub fn process_frame(&mut self, chunk: Vec<u8>) -> Vec<Vec<u8>> {
        if self.continuous_mode {
            return vec![chunk];
        }

        if !self.is_active {
            let triggered = if self.manual_armed {
                self.speech_engine
                    .as_mut()
                    .is_some_and(|v| v.process(&chunk))
            } else {
                self.engine.process(&chunk)
            };

            self.buffered_bytes += chunk.len();
            self.pre_roll_buffer.push_back(chunk.clone());

            // Evict oldest chunks when buffer exceeds requested time window
            while self.buffered_bytes > self.max_pre_roll_bytes && self.pre_roll_buffer.len() > 1 {
                if let Some(old) = self.pre_roll_buffer.pop_front() {
                    self.buffered_bytes = self.buffered_bytes.saturating_sub(old.len());
                }
            }

            if triggered {
                self.is_active = true;
                self.has_spoken = false;
                self.silence_bytes = 0;
                let outgoing: Vec<Vec<u8>> = self.pre_roll_buffer.drain(..).collect();
                self.buffered_bytes = 0;
                if outgoing.is_empty() {
                    vec![chunk]
                } else {
                    outgoing
                }
            } else {
                Vec::new()
            }
        } else {
            let voice_active = match self.speech_engine.as_mut() {
                Some(vad) => vad.process(&chunk),
                None => self.engine.process(&chunk),
            };
            if voice_active {
                self.has_spoken = true;
                self.silence_bytes = 0;
            } else {
                self.silence_bytes += chunk.len();
                let limit = if self.has_spoken {
                    self.max_silence_bytes
                } else {
                    self.max_initial_silence_bytes
                };
                if self.silence_bytes >= limit {
                    self.is_active = false;
                    self.has_spoken = false;
                    self.engine.reset();
                    if let Some(vad) = &mut self.speech_engine {
                        vad.reset();
                    }
                    self.pre_roll_buffer.clear();
                    self.buffered_bytes = 0;
                    self.silence_bytes = 0;
                }
            }
            vec![chunk]
        }
    }

    /// Check if currently in active streaming state
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    /// Manually reset gate to inactive (e.g. at end of turn)
    #[allow(dead_code)]
    pub fn reset_gate(&mut self) {
        if !self.continuous_mode {
            self.is_active = false;
            self.has_spoken = false;
            self.engine.reset();
            if let Some(vad) = &mut self.speech_engine {
                vad.reset();
            }
            self.manual_armed = false;
            self.pre_roll_buffer.clear();
            self.buffered_bytes = 0;
            self.silence_bytes = 0;
        }
    }

    /// Hot-reloads the wake-word engine without interrupting audio loop.
    pub fn reload_engine(&mut self, new_engine: Box<dyn WakeWordEngine>) {
        self.engine = new_engine;
        self.reset_gate();
    }
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
    fn test_vad_sustains_speech_across_micropauses() {
        let mut vad = SimpleVadEngine::new(100.0);
        let loud_sample = (0..256)
            .flat_map(|_| 500i16.to_le_bytes())
            .collect::<Vec<u8>>();
        let quiet_sample = (0..256)
            .flat_map(|_| 50i16.to_le_bytes())
            .collect::<Vec<u8>>();

        // Needs 3 consecutive loud frames for onset
        assert!(!vad.process(&loud_sample));
        assert!(!vad.process(&loud_sample));
        assert!(vad.process(&loud_sample)); // Now speaking!

        // Micro-pause (single quiet frame)
        assert!(!vad.process(&quiet_sample));

        // Next frame is loud: must sustain speech immediately without needing 3 frames again!
        assert!(vad.process(&loud_sample));

        // Two quiet frames
        assert!(!vad.process(&quiet_sample));
        assert!(!vad.process(&quiet_sample));

        // Next loud frame sustains speech immediately!
        assert!(vad.process(&loud_sample));

        // Reset clears speaking state
        vad.reset();
        // After reset, onset requires 3 consecutive frames again
        assert!(!vad.process(&loud_sample));
        assert!(!vad.process(&loud_sample));
        assert!(vad.process(&loud_sample));
    }

    #[test]
    fn test_always_active_engine() {
        let mut engine = AlwaysActiveEngine;
        assert!(engine.process(&[]));
        assert!(engine.process(&[1, 2, 3, 4]));
        engine.reset();
        assert!(engine.process(&[]));
    }

    #[test]
    fn test_input_gate_suppresses_ambient_audio_until_activated() {
        let engine = Box::new(SimpleVadEngine::new(1000.0));
        // 100ms pre-roll, 160ms silence timeout at 16kHz (32 bytes/ms -> 3200 bytes pre-roll, 5120 bytes silence)
        let mut gate = InputGate::new(engine, 100, 160, 16000, false);
        assert!(!gate.is_active());

        // Feed silence chunks (512 bytes = 16ms each): nothing emitted
        let silence = vec![0u8; 512];
        for _ in 0..10 {
            let emitted = gate.process_frame(silence.clone());
            assert!(emitted.is_empty());
        }
        assert!(!gate.is_active());

        // Feed loud frames to trigger VAD
        let mut loud_frame = Vec::new();
        for i in 0..256 {
            let sample: i16 = if i % 2 == 0 { 5000 } else { -5000 };
            loud_frame.extend_from_slice(&sample.to_le_bytes());
        }

        assert!(gate.process_frame(loud_frame.clone()).is_empty());
        assert!(gate.process_frame(loud_frame.clone()).is_empty());
        // Frame 3 triggers! It must flush pre-roll buffer (including earlier frames)
        let emitted = gate.process_frame(loud_frame.clone());
        assert!(!emitted.is_empty());
        assert!(gate.is_active());

        // Next frame is emitted directly
        let emitted_next = gate.process_frame(loud_frame.clone());
        assert_eq!(emitted_next.len(), 1);

        // Feed 10 silence chunks (160ms = 5120 bytes, exactly meeting silence timeout)
        for _ in 0..10 {
            let _ = gate.process_frame(silence.clone());
        }
        // After silence timeout, gate returns to inactive
        assert!(!gate.is_active());
    }

    #[test]
    fn test_input_gate_continuous_mode_passes_everything() {
        let engine = Box::new(AlwaysActiveEngine);
        let mut gate = InputGate::new(engine, 100, 200, 16000, true);
        assert!(gate.is_active());

        let silence = vec![0u8; 512];
        let emitted = gate.process_frame(silence.clone());
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0], silence);
    }

    #[test]
    fn test_input_gate_chunk_size_invariance() {
        let engine = Box::new(SimpleVadEngine::new(1000.0));
        // 200ms pre-roll (6400 bytes at 16kHz)
        let mut gate = InputGate::new(engine, 200, 300, 16000, false);

        // Push 10 small 256-byte chunks (total 2560 bytes < 6400 max)
        for _ in 0..10 {
            assert!(gate.process_frame(vec![0u8; 256]).is_empty());
        }
        assert_eq!(gate.buffered_bytes, 2560);

        // Push 20 more 256-byte chunks: buffer caps at max_pre_roll_bytes (6400)
        for _ in 0..20 {
            assert!(gate.process_frame(vec![0u8; 256]).is_empty());
        }
        assert!(gate.buffered_bytes <= 6400 + 256);
    }
    #[test]
    fn arbitrary_loud_audio_without_activation_never_leaves_gate() {
        let mut gate =
            InputGate::with_activation(Box::new(keyword::DisabledKeyword), 10.0, 400, 1000, 16000);
        let speech: Vec<u8> = (0..256).flat_map(|_| 10000i16.to_le_bytes()).collect();
        for _ in 0..300 {
            assert!(gate.process_frame(speech.clone()).is_empty());
        }
        assert!(!gate.is_active());
    }

    #[test]
    fn wake_detector_is_not_used_as_speech_detector() {
        struct Once(bool);
        impl WakeWordEngine for Once {
            fn process(&mut self, _: &[u8]) -> bool {
                let hit = self.0;
                self.0 = false;
                hit
            }
            fn reset(&mut self) {
                self.0 = false;
            }
        }
        let mut gate = InputGate::with_activation(Box::new(Once(true)), 10.0, 400, 700, 16000);
        let speech: Vec<u8> = (0..256).flat_map(|_| 1000i16.to_le_bytes()).collect();
        for _ in 0..150 {
            assert!(!gate.process_frame(speech.clone()).is_empty());
        }
        assert!(
            gate.is_active(),
            "speech must not be cut off 700ms after the keyword"
        );
        for _ in 0..50 {
            gate.process_frame(vec![0; 512]);
        }
        assert!(!gate.is_active());
    }

    #[test]
    fn nonconsecutive_noise_impulses_do_not_accumulate_into_speech() {
        let mut vad = SimpleVadEngine::new(10.0);
        let impulse: Vec<u8> = (0..256).flat_map(|_| 5000i16.to_le_bytes()).collect();
        for _ in 0..50 {
            assert!(!vad.process(&impulse));
            assert!(!vad.process(&[0; 512]));
        }
    }

    #[test]
    fn test_input_gate_initial_silence_timeout_and_utterance_timeout() {
        struct TriggerOnce(bool);
        impl WakeWordEngine for TriggerOnce {
            fn process(&mut self, _: &[u8]) -> bool {
                let val = self.0;
                self.0 = false;
                val
            }
            fn reset(&mut self) {
                self.0 = false;
            }
        }

        // At 16kHz 16-bit mono, 1ms = 32 bytes.
        // 500ms initial silence = 16000 bytes.
        // 200ms utterance silence = 6400 bytes.
        // Chunk of 320 bytes = 10ms.
        let mut gate = InputGate::with_activation_and_initial_timeout(
            Box::new(TriggerOnce(true)),
            10.0,
            100,
            500,
            200,
            16000,
        );

        let silence_chunk = vec![0u8; 320]; // 10ms of silence
        let speech_chunk: Vec<u8> = (0..160).flat_map(|_| 5000i16.to_le_bytes()).collect(); // 10ms of loud speech

        // First frame triggers wakeword
        let out = gate.process_frame(silence_chunk.clone());
        assert!(!out.is_empty());
        assert!(gate.is_active());

        // User stays silent for 30 chunks = 300ms (< 500ms initial limit).
        for _ in 0..30 {
            let _ = gate.process_frame(silence_chunk.clone());
            assert!(
                gate.is_active(),
                "Gate must remain active during initial formulation window"
            );
        }

        // User speaks for 10 chunks = 100ms.
        for _ in 0..10 {
            let _ = gate.process_frame(speech_chunk.clone());
            assert!(gate.is_active());
        }

        // Now user stops speaking. Silence starts.
        // 15 chunks = 150ms (< 200ms utterance silence limit).
        for _ in 0..15 {
            let _ = gate.process_frame(silence_chunk.clone());
            assert!(
                gate.is_active(),
                "Gate must remain active within utterance silence limit"
            );
        }

        // 10 more chunks = 100ms (total 250ms > 200ms).
        for _ in 0..10 {
            let _ = gate.process_frame(silence_chunk.clone());
        }
        assert!(
            !gate.is_active(),
            "Gate must close after utterance silence limit"
        );
    }
}
