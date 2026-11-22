pub mod enrollment;
pub mod keyword;
use tracing::debug;

/// Numeric matcher metadata only; never includes microphone samples or recognized text.
#[derive(Debug, Clone, Copy)]
pub struct KeywordObservation {
    pub score: f32,
    pub avg_score: f32,
    #[allow(dead_code)]
    pub counter: usize,
    #[allow(dead_code)]
    pub gain: f32,
    pub detected: bool,
}

pub trait WakeWordEngine: Send + Sync {
    /// Process incoming 16-bit 16kHz PCM audio frame (e.g. 512 bytes / 256 samples).
    /// Returns true if wake word was detected in this frame.
    fn process(&mut self, pcm_chunk: &[u8]) -> bool;

    /// Reset internal state
    fn reset(&mut self);

    /// Hot-update calibration settings on the fly
    fn update_calibration(&mut self, _cfg: &rustpotter::PersonalDetectorConfig) {}

    /// Drain the most recent numeric matcher observation, if available.
    fn take_observation(&mut self) -> Option<KeywordObservation> {
        None
    }
}

/// Energy-based speech segmentation within an explicitly authorized input window.
/// This detector never grants keyword activation in the production InputGate.
pub struct SimpleVadEngine {
    base_threshold_rms: f32,
    noise_floor_rms: f32,
    consecutive_active: usize,
    required_consecutive: usize,
    is_speaking: bool,
    current_frame_active: bool,
    noise_adaptation: bool,
    noise_multiplier: f32,
    noise_offset: f32,
    rise_alpha: f32,
    fall_alpha: f32,
}

impl SimpleVadEngine {
    pub fn new(threshold_rms: f32) -> Self {
        Self {
            base_threshold_rms: threshold_rms,
            noise_floor_rms: threshold_rms * 0.5,
            consecutive_active: 0,
            required_consecutive: 3, // ~48ms of audio for onset
            is_speaking: false,
            current_frame_active: false,
            noise_adaptation: true,
            noise_multiplier: 1.15,
            noise_offset: 10.0,
            rise_alpha: 0.02,
            fall_alpha: 0.10,
        }
    }

    /// Construct from settings already validated at the configuration/API boundary.
    pub fn from_settings(settings: &crate::config::WakewordCalibrationConfig) -> Self {
        Self {
            base_threshold_rms: settings.vad_threshold,
            noise_floor_rms: settings.vad_threshold * 0.5,
            consecutive_active: 0,
            required_consecutive: settings.vad_onset_frames,
            is_speaking: false,
            current_frame_active: false,
            noise_adaptation: settings.vad_noise_adaptation,
            noise_multiplier: settings.vad_noise_multiplier,
            noise_offset: settings.vad_noise_offset,
            rise_alpha: settings.vad_rise_alpha,
            fall_alpha: settings.vad_fall_alpha,
        }
    }

    fn dynamic_threshold(&self) -> f32 {
        if self.noise_adaptation {
            (self.noise_floor_rms * self.noise_multiplier + self.noise_offset)
                .max(self.base_threshold_rms)
        } else {
            self.base_threshold_rms
        }
    }

    fn reset_onset(&mut self) {
        self.consecutive_active = 0;
        self.is_speaking = false;
        self.current_frame_active = false;
    }

    /// Current floor, actual decision threshold and frame activity; no audio is retained.
    pub fn diagnostics(&self) -> (f32, f32, bool) {
        (
            self.noise_floor_rms,
            self.dynamic_threshold(),
            self.current_frame_active,
        )
    }
}

impl WakeWordEngine for SimpleVadEngine {
    fn process(&mut self, pcm_chunk: &[u8]) -> bool {
        self.current_frame_active = false;
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

        // Dynamic noise floor tracking:
        // When not speaking, rapidly track downward or steadily adapt upward.
        // When speaking, only track downward so speech doesn't inflate the noise floor.
        if self.noise_adaptation {
            let alpha = if rms < self.noise_floor_rms {
                self.fall_alpha
            } else if !self.is_speaking {
                self.rise_alpha
            } else {
                0.0
            };
            self.noise_floor_rms += alpha * (rms - self.noise_floor_rms);
        }

        let dynamic_threshold = self.dynamic_threshold();

        if rms > dynamic_threshold {
            if self.is_speaking {
                self.current_frame_active = true;
                return true;
            }
            self.consecutive_active += 1;
            if self.consecutive_active >= self.required_consecutive {
                self.is_speaking = true;
                self.current_frame_active = true;
                debug!(
                    "VAD voice detected (RMS: {:.1} > dynamic: {:.1}, floor: {:.1})",
                    rms, dynamic_threshold, self.noise_floor_rms
                );
                return true;
            }
        } else {
            self.consecutive_active = 0;
        }

        false
    }

    fn reset(&mut self) {
        self.reset_onset();
        self.noise_floor_rms = self.base_threshold_rms * 0.5;
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
    speech_engine: Option<SimpleVadEngine>,
    manual_armed: bool,
    is_active: bool,
    has_spoken: bool,
    had_spoken_last_turn: bool,
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
            had_spoken_last_turn: false,
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
        gate.speech_engine = Some(SimpleVadEngine::new(vad_threshold));
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
        if armed != self.manual_armed && !self.is_active {
            if let Some(vad) = &mut self.speech_engine {
                // A newly granted input window must not reuse onset evidence from
                // earlier room speech. Keep the measured noise floor.
                vad.reset_onset();
            }
        }
        self.manual_armed = armed;
    }

    /// Process an incoming PCM frame from the microphone.
    /// Returns audio chunks to forward to the cloud API, or empty Vec if gated.
    pub fn process_frame(&mut self, chunk: Vec<u8>) -> Vec<Vec<u8>> {
        if self.continuous_mode {
            return vec![chunk];
        }

        // Observe room energy locally even while gated so diagnostics reflect the
        // room. Only an explicit manual window may use this result to open input.
        let speech_active = self
            .speech_engine
            .as_mut()
            .is_some_and(|vad| vad.process(&chunk));

        if !self.is_active {
            let triggered = if self.manual_armed {
                speech_active
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
                self.has_spoken = self.manual_armed;
                self.had_spoken_last_turn = true;
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
                Some(_) => speech_active,
                None => self.engine.process(&chunk),
            };
            if voice_active {
                self.has_spoken = true;
                self.had_spoken_last_turn = true;
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

    /// Check if any speech has been detected in current turn or when turn just closed
    pub fn has_spoken(&self) -> bool {
        self.has_spoken || self.had_spoken_last_turn
    }

    /// Manually reset gate to inactive (e.g. at end of turn)
    #[allow(dead_code)]
    pub fn reset_gate(&mut self) {
        if !self.continuous_mode {
            self.is_active = false;
            self.has_spoken = false;
            self.had_spoken_last_turn = false;
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

    /// Clear pre-roll buffer to prevent acoustic echo leakage (e.g. after TTS playback or loud noises)
    pub fn sanitize_pre_roll(&mut self) {
        self.pre_roll_buffer.clear();
        self.buffered_bytes = 0;
    }

    /// Hot-reloads the wake-word engine without interrupting audio loop.
    pub fn reload_engine(&mut self, new_engine: Box<dyn WakeWordEngine>) {
        self.engine = new_engine;
        self.reset_gate();
    }

    /// Hot-updates the detector calibration on the fly without interrupting audio loop.
    #[allow(dead_code)]
    pub fn update_calibration(&mut self, cfg: &rustpotter::PersonalDetectorConfig) {
        self.engine.update_calibration(cfg);
    }

    /// Hot-updates VAD speech detection threshold.
    #[allow(dead_code)]
    pub fn update_vad_threshold(&mut self, threshold_rms: f32) {
        self.speech_engine = Some(SimpleVadEngine::new(threshold_rms));
    }

    /// Atomically replace the gate's validated 16 kHz PCM16 acoustic settings.
    /// Pending onset/pre-roll is discarded so old evidence cannot trigger a new profile.
    pub fn apply_acoustic_settings(&mut self, settings: &crate::config::WakewordCalibrationConfig) {
        self.reset_gate();
        self.engine
            .update_calibration(&settings.to_detector_config());
        self.speech_engine = Some(SimpleVadEngine::from_settings(settings));
        const BYTES_PER_MS: usize = 16_000 * 2 / 1000;
        self.max_pre_roll_bytes = settings.pre_roll_ms as usize * BYTES_PER_MS;
        self.max_silence_bytes = settings.silence_timeout_ms as usize * BYTES_PER_MS;
        self.max_initial_silence_bytes =
            settings.initial_silence_timeout_ms as usize * BYTES_PER_MS;
    }

    /// Floor RMS, effective VAD threshold RMS and current frame activity.
    pub fn vad_diagnostics(&self) -> (f32, f32, bool) {
        self.speech_engine
            .as_ref()
            .map_or((0.0, 0.0, false), SimpleVadEngine::diagnostics)
    }

    /// Drain the keyword observation without logging or retaining microphone audio.
    pub fn take_keyword_observation(&mut self) -> Option<KeywordObservation> {
        self.engine.take_observation()
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
        assert!(
            gate.has_spoken(),
            "Gate must retain has_spoken=true after utterance ends"
        );
    }

    #[test]
    fn test_input_gate_initial_silence_after_wakeword_retains_has_spoken() {
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

        let mut gate = InputGate::with_activation_and_initial_timeout(
            Box::new(TriggerOnce(true)),
            10.0,
            100,
            300,
            200,
            16000,
        );

        let silence_chunk = vec![0u8; 320];

        let out = gate.process_frame(silence_chunk.clone());
        assert!(!out.is_empty());
        assert!(gate.is_active());
        assert!(gate.has_spoken());

        for _ in 0..30 {
            let _ = gate.process_frame(silence_chunk.clone());
        }

        assert!(
            !gate.is_active(),
            "Gate must close after initial silence timeout"
        );
        assert!(
            gate.has_spoken(),
            "Gate must report has_spoken=true so wake-word greeting is processed by cloud"
        );
    }

    #[test]
    fn test_sanitize_pre_roll_clears_buffered_audio() {
        struct NeverTrigger;
        impl WakeWordEngine for NeverTrigger {
            fn process(&mut self, _: &[u8]) -> bool {
                false
            }
            fn reset(&mut self) {}
        }

        let mut gate = InputGate::with_activation(Box::new(NeverTrigger), 10.0, 400, 1000, 16000);
        let chunk = vec![0x42; 512];

        // Feed some frames while inactive
        for _ in 0..5 {
            assert!(gate.process_frame(chunk.clone()).is_empty());
        }
        assert_eq!(gate.buffered_bytes, 5 * 512);

        // Sanitize pre-roll (e.g. after TTS or during echo)
        gate.sanitize_pre_roll();
        assert_eq!(gate.buffered_bytes, 0);
        assert!(gate.pre_roll_buffer.is_empty());
    }

    #[test]
    fn manual_armed_waits_for_speech_and_never_opens_on_silence() {
        struct NeverTrigger;
        impl WakeWordEngine for NeverTrigger {
            fn process(&mut self, _: &[u8]) -> bool {
                false
            }
            fn reset(&mut self) {}
        }

        let mut gate = InputGate::with_activation(Box::new(NeverTrigger), 10.0, 400, 1000, 16000);
        let chunk = vec![0x00; 512];
        let speech: Vec<u8> = (0..256).flat_map(|_| 5000i16.to_le_bytes()).collect();

        // Without manual arm, inactive
        assert!(gate.process_frame(chunk.clone()).is_empty());
        assert!(!gate.is_active());

        // Arming grants a bounded chance to speak; it is not itself speech.
        gate.set_manual_armed(true);
        for _ in 0..500 {
            assert!(gate.process_frame(chunk.clone()).is_empty());
            assert!(!gate.has_spoken());
        }
        assert!(!gate.is_active());

        // Isolated sound bursts cannot accumulate onset across quiet frames.
        for _ in 0..10 {
            assert!(gate.process_frame(speech.clone()).is_empty());
            assert!(gate.process_frame(chunk.clone()).is_empty());
        }
        assert!(gate.process_frame(speech.clone()).is_empty());
        assert!(gate.process_frame(speech.clone()).is_empty());
        let out = gate.process_frame(speech);
        assert!(!out.is_empty());
        assert!(gate.is_active());
        assert!(gate.has_spoken());
    }

    #[test]
    fn new_manual_window_does_not_reuse_ambient_speech_onset() {
        let mut gate =
            InputGate::with_activation(Box::new(keyword::DisabledKeyword), 10.0, 100, 1000, 16000);
        let speech: Vec<u8> = (0..256).flat_map(|_| 5000i16.to_le_bytes()).collect();
        for _ in 0..10 {
            assert!(gate.process_frame(speech.clone()).is_empty());
        }

        gate.set_manual_armed(true);
        assert!(gate.process_frame(speech.clone()).is_empty());
        assert!(gate.process_frame(speech.clone()).is_empty());
        assert!(!gate.process_frame(speech).is_empty());
    }

    #[test]
    fn revoked_manual_window_cannot_leave_partial_onset_authorized() {
        let mut gate =
            InputGate::with_activation(Box::new(keyword::DisabledKeyword), 10.0, 100, 1000, 16000);
        let speech: Vec<u8> = (0..256).flat_map(|_| 5000i16.to_le_bytes()).collect();
        gate.set_manual_armed(true);
        assert!(gate.process_frame(speech.clone()).is_empty());
        assert!(gate.process_frame(speech.clone()).is_empty());

        gate.set_manual_armed(false);
        for _ in 0..5 {
            assert!(gate.process_frame(speech.clone()).is_empty());
        }
        assert!(!gate.is_active());
        gate.set_manual_armed(true);
        assert!(gate.process_frame(speech.clone()).is_empty());
        assert!(gate.process_frame(speech.clone()).is_empty());
        assert!(!gate.process_frame(speech).is_empty());
    }

    #[test]
    fn calibration_changes_vad_and_timings_without_retaining_old_audio() {
        let mut gate =
            InputGate::with_activation(Box::new(keyword::DisabledKeyword), 10.0, 400, 1000, 16000);
        let quiet_speech: Vec<u8> = (0..160).flat_map(|_| 500i16.to_le_bytes()).collect();
        gate.set_manual_armed(true);
        for _ in 0..3 {
            gate.process_frame(quiet_speech.clone());
        }
        assert!(gate.is_active());

        let settings = crate::config::WakewordCalibrationConfig {
            vad_threshold: 1000.0,
            vad_noise_adaptation: false,
            vad_onset_frames: 1,
            pre_roll_ms: 0,
            initial_silence_timeout_ms: 500,
            silence_timeout_ms: 200,
            ..Default::default()
        };
        gate.apply_acoustic_settings(&settings);
        assert!(!gate.is_active());
        assert!(!gate.has_spoken());
        assert!(gate.process_frame(quiet_speech.clone()).is_empty());
        gate.set_manual_armed(true);
        assert!(gate.process_frame(quiet_speech).is_empty());

        let speech: Vec<u8> = (0..160).flat_map(|_| 2000i16.to_le_bytes()).collect();
        // No old frames from the earlier profile leak into the newly opened turn.
        assert_eq!(gate.process_frame(speech.clone()), vec![speech]);
        for _ in 0..19 {
            gate.process_frame(vec![0; 320]);
            assert!(gate.is_active());
        }
        gate.process_frame(vec![0; 320]);
        assert!(!gate.is_active());
        assert!(gate.has_spoken());
    }

    #[test]
    fn sensitive_vad_settings_never_replace_keyword_activation() {
        let mut gate =
            InputGate::with_activation(Box::new(keyword::DisabledKeyword), 10.0, 400, 1000, 16000);
        gate.apply_acoustic_settings(&crate::config::WakewordCalibrationConfig {
            vad_threshold: 1.0,
            vad_noise_adaptation: false,
            vad_onset_frames: 1,
            ..Default::default()
        });
        let speech: Vec<u8> = (0..256).flat_map(|_| 5000i16.to_le_bytes()).collect();
        for _ in 0..100 {
            assert!(gate.process_frame(speech.clone()).is_empty());
        }
        assert!(!gate.is_active());
        assert!(!gate.has_spoken());
    }

    #[test]
    fn vad_settings_control_adaptation_and_current_frame_diagnostics() {
        let settings = crate::config::WakewordCalibrationConfig {
            vad_threshold: 100.0,
            vad_noise_adaptation: false,
            vad_onset_frames: 2,
            ..Default::default()
        };
        let mut vad = SimpleVadEngine::from_settings(&settings);
        let speech: Vec<u8> = (0..256).flat_map(|_| 500i16.to_le_bytes()).collect();
        assert!(!vad.process(&speech));
        assert!(vad.process(&speech));
        assert_eq!(vad.diagnostics(), (50.0, 100.0, true));
        assert!(!vad.process(&[0; 512]));
        assert_eq!(vad.diagnostics(), (50.0, 100.0, false));

        // With a deliberately fast adaptive floor the same steady level is
        // classified as background rather than repeated speech onset.
        let mut adaptive =
            SimpleVadEngine::from_settings(&crate::config::WakewordCalibrationConfig {
                vad_noise_adaptation: true,
                vad_noise_multiplier: 2.0,
                vad_noise_offset: 0.0,
                vad_rise_alpha: 1.0,
                ..settings
            });
        for _ in 0..10 {
            assert!(!adaptive.process(&speech));
        }
        let (floor, threshold, active) = adaptive.diagnostics();
        assert_eq!(floor, 500.0);
        assert_eq!(threshold, 1000.0);
        assert!(!active);
    }
}
