//! Wake-Word enrollment engine for GPT Mini.
//!
//! Provides acoustic sample decoding, energy trimming, quality control,
//! template down-selection (for Rustpotter's 3..=8 template limit),
//! and atomic `.rpw` model compilation.

use anyhow::{bail, ensure, Context, Result};
use rustpotter::{WakewordRef, WakewordRefBuildFromBuffers, WakewordSave};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::info;

pub const SAMPLE_RATE: usize = 16_000;
pub const MIN_SAMPLES: usize = 4_800; // 300 ms
pub const MAX_SAMPLES: usize = 48_000; // 3000 ms
pub const TARGET_STEPS: usize = 25;
pub const MIN_STEPS: usize = 3;
pub const MAX_TEMPLATES: usize = 8;

/// Audio tap for hardware microphone recording during enrollment.
#[derive(Clone)]
pub struct HardwareRecorder {
    state: Arc<Mutex<Option<RecorderRequest>>>,
}

struct RecorderRequest {
    target_samples: usize,
    collected: Vec<i16>,
    sender: Option<oneshot::Sender<Vec<i16>>>,
}

impl Default for HardwareRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareRecorder {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.state.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Taps into incoming PCM chunks from the hardware microphone.
    /// Non-blocking check: if no recording is requested, returns immediately.
    pub fn feed_pcm(&self, chunk: &[u8]) {
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(req) = guard.as_mut() {
            let (chunks, _) = chunk.as_chunks::<2>();
            for c in chunks {
                req.collected.push(i16::from_le_bytes(*c));
            }
            if req.collected.len() >= req.target_samples {
                if let Some(tx) = req.sender.take() {
                    let _ = tx.send(std::mem::take(&mut req.collected));
                }
                *guard = None;
            }
        }
    }

    /// Asynchronously requests a recording of specified duration (e.g. 2.5s) from hardware mic.
    pub async fn record(&self, duration: Duration) -> Result<Vec<i16>> {
        let (tx, rx) = oneshot::channel();
        let target_samples = (SAMPLE_RATE as f64 * duration.as_secs_f64()).round() as usize;
        {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;
            *guard = Some(RecorderRequest {
                target_samples,
                collected: Vec::with_capacity(target_samples),
                sender: Some(tx),
            });
        }
        match tokio::time::timeout(duration + Duration::from_secs(3), rx).await {
            Ok(Ok(samples)) => Ok(samples),
            Ok(Err(_)) => bail!("Hardware recording channel closed unexpectedly"),
            Err(_) => {
                let mut guard = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;
                *guard = None;
                bail!("Hardware recording timed out (microphone quiet or inactive)");
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrollmentSample {
    pub id: usize,
    pub phase: u8,
    pub phase_name: String,
    pub duration_ms: u32,
    pub peak_rms: f32,
    pub snr: f32,
    #[serde(skip)]
    pub pcm: Vec<i16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrollmentSession {
    pub keyword_name: String,
    pub samples: Vec<EnrollmentSample>,
    pub target_steps: usize,
    pub min_steps: usize,
    pub current_step: usize,
    pub can_compile: bool,
}

impl EnrollmentSession {
    pub fn new(keyword_name: String) -> Self {
        Self {
            keyword_name,
            samples: Vec::new(),
            target_steps: TARGET_STEPS,
            min_steps: MIN_STEPS,
            current_step: 1,
            can_compile: false,
        }
    }

    pub fn phase_for_step(step: usize) -> (u8, &'static str) {
        match step {
            1..=8 => (1, "Ближняя зона (0.5–1 м, спокойный тон)"),
            9..=16 => (2, "Средняя дистанция (2–3 м, обычная речь)"),
            17..=21 => (3, "Громкий призыв (3–4 м, эмоциональный зов)"),
            _ => (4, "Тихий вечерний голос / полушёпот"),
        }
    }

    pub fn add_pcm_sample(&mut self, raw_pcm: &[i16]) -> Result<EnrollmentSample> {
        let (phase, phase_name) = Self::phase_for_step(self.current_step);
        let sample = process_sample(raw_pcm, self.samples.len() + 1, phase, phase_name)?;
        self.samples.push(sample.clone());
        self.current_step = (self.samples.len() + 1).min(self.target_steps);
        self.can_compile = self.samples.len() >= self.min_steps;
        Ok(sample)
    }

    pub fn remove_sample(&mut self, id: usize) -> bool {
        let initial_len = self.samples.len();
        self.samples.retain(|s| s.id != id);
        // Re-index remaining samples
        for (i, s) in self.samples.iter_mut().enumerate() {
            s.id = i + 1;
            let (phase, name) = Self::phase_for_step(s.id);
            s.phase = phase;
            s.phase_name = name.to_string();
        }
        self.current_step = (self.samples.len() + 1).min(self.target_steps);
        self.can_compile = self.samples.len() >= self.min_steps;
        self.samples.len() < initial_len
    }

    pub fn compile_model(&self, output_path: &Path) -> Result<WakewordCompileResult> {
        ensure!(
            self.samples.len() >= self.min_steps,
            "Необходимо как минимум {} сэмпла для обучения (записано {})",
            self.min_steps,
            self.samples.len()
        );

        compile_reference(&self.keyword_name, &self.samples, output_path)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WakewordCompileResult {
    pub keyword_name: String,
    pub total_samples_recorded: usize,
    pub templates_used: usize,
    pub model_size_bytes: usize,
    pub output_path: String,
}

/// Decodes mono 16kHz PCM16 from a standard WAV byte stream.
pub fn decode_wav_pcm16(bytes: &[u8]) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::new(Cursor::new(bytes)).context("Invalid WAV data")?;
    let spec = reader.spec();
    ensure!(
        spec.channels == 1
            && spec.sample_rate == 16_000
            && spec.bits_per_sample == 16
            && spec.sample_format == hound::SampleFormat::Int,
        "Expected mono 16kHz PCM16 WAV, got ch={} rate={} bits={}",
        spec.channels,
        spec.sample_rate,
        spec.bits_per_sample
    );
    let mut samples = Vec::with_capacity(reader.len() as usize);
    for s in reader.samples::<i16>() {
        samples.push(s.context("Malformed PCM sample")?);
    }
    Ok(samples)
}

/// Encodes raw PCM16 samples into a standard mono 16kHz WAV byte stream.
pub fn encode_wav_pcm16(samples: &[i16]) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(
            &mut cursor,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        for &s in samples {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

/// Trims leading and trailing silence using dynamic RMS energy threshold.
pub fn trim_enrollment(samples: &[i16]) -> Result<Vec<i16>> {
    ensure!(!samples.is_empty(), "Empty audio buffer");

    let rms: Vec<f64> = samples
        .chunks(160)
        .map(|frame| {
            (frame.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / frame.len() as f64).sqrt()
        })
        .collect();

    ensure!(!rms.is_empty(), "Audio too short for analysis");

    let mut sorted = rms.clone();
    sorted.sort_by(f64::total_cmp);

    let noise = sorted[sorted.len() / 10];
    let peak = *sorted.last().context("Empty recording")?;

    ensure!(
        peak >= 1.0 && peak >= noise * 1.5,
        "В записи не обнаружено отчётливого голоса (слишком тихо или фоновый шум)"
    );

    let threshold = (peak * 0.25).max(noise * 2.5).min(peak * 0.65);

    let peak_index = rms
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .context("Empty recording")?;

    let mut first = peak_index;
    for i in (0..peak_index).rev() {
        if rms[i] >= threshold {
            first = i;
        }
        if first.saturating_sub(i) >= 12 {
            break;
        }
    }

    let mut last = peak_index;
    for (i, value) in rms.iter().enumerate().skip(peak_index + 1) {
        if *value >= threshold {
            last = i;
        }
        if i.saturating_sub(last) >= 12 {
            break;
        }
    }

    // Add 8 frames (80 ms) before and 12 frames (120 ms) after for natural envelope
    let start = first.saturating_sub(8) * 160;
    let end = ((last + 1 + 12) * 160).min(samples.len());

    let trimmed = &samples[start..end];
    ensure!(
        trimmed.len() >= MIN_SAMPLES,
        "Произнесённое слово слишком короткое ({:.1} с). Говорите чётко не менее 0.3 с.",
        trimmed.len() as f64 / 16000.0
    );
    ensure!(
        trimmed.len() <= MAX_SAMPLES,
        "Запись слишком длинная ({:.1} с). Произносите только слово вызова (до 3 сек).",
        trimmed.len() as f64 / 16000.0
    );

    Ok(trimmed.to_vec())
}

/// Evaluates a raw recording, trims silence, calculates metrics, and returns an `EnrollmentSample`.
pub fn process_sample(
    raw_pcm: &[i16],
    id: usize,
    phase: u8,
    phase_name: &str,
) -> Result<EnrollmentSample> {
    ensure!(
        raw_pcm.len() >= 1600,
        "Запись слишком короткая для анализа"
    );

    // Calculate overall peak and noise floor
    let mut sum_sq = 0.0f64;
    let mut max_abs = 0i16;
    for &s in raw_pcm {
        let abs = s.abs();
        if abs > max_abs {
            max_abs = abs;
        }
        sum_sq += f64::from(s).powi(2);
    }
    let total_rms = (sum_sq / raw_pcm.len() as f64).sqrt() as f32;

    // Check for extreme digital clipping
    ensure!(
        max_abs < 32700,
        "Запись искажена клиппингом (слишком близко или громко). Повторите спокойнее."
    );

    let trimmed = trim_enrollment(raw_pcm)?;
    let duration_ms = (trimmed.len() as f64 / 16.0).round() as u32;

    // Quality check SNR
    let mut trimmed_sum_sq = 0.0f64;
    for &s in &trimmed {
        trimmed_sum_sq += f64::from(s).powi(2);
    }
    let trimmed_rms = (trimmed_sum_sq / trimmed.len() as f64).sqrt() as f32;
    let noise_rms = total_rms.min(trimmed_rms * 0.3).max(1.0);
    let snr = 20.0 * (trimmed_rms / noise_rms).log10();

    Ok(EnrollmentSample {
        id,
        phase,
        phase_name: phase_name.to_string(),
        duration_ms,
        peak_rms: trimmed_rms,
        snr: if snr.is_finite() { snr } else { 12.0 },
        pcm: trimmed,
    })
}

/// Selects up to `MAX_TEMPLATES` (8) best, most diverse templates across recorded samples.
/// This fulfills Rustpotter's strict 3..=8 personal reference constraint even when user records 25 steps.
pub fn select_best_templates(samples: &[EnrollmentSample]) -> Vec<&EnrollmentSample> {
    if samples.len() <= MAX_TEMPLATES {
        return samples.iter().collect();
    }

    // Group samples by phase
    let mut by_phase: HashMap<u8, Vec<&EnrollmentSample>> = HashMap::new();
    for s in samples {
        by_phase.entry(s.phase).or_default().push(s);
    }

    // Sort each phase by SNR descending
    for list in by_phase.values_mut() {
        list.sort_by(|a, b| b.snr.total_cmp(&a.snr));
    }

    let mut selected: Vec<&EnrollmentSample> = Vec::new();
    let phases = [1u8, 2, 3, 4];

    // Pick top samples round-robin from each phase
    let mut round = 0;
    while selected.len() < MAX_TEMPLATES {
        let mut added_in_round = false;
        for phase in &phases {
            if selected.len() >= MAX_TEMPLATES {
                break;
            }
            if let Some(list) = by_phase.get(phase) {
                if let Some(&sample) = list.get(round) {
                    selected.push(sample);
                    added_in_round = true;
                }
            }
        }
        if !added_in_round {
            break;
        }
        round += 1;
    }

    // If still less than MAX_TEMPLATES and we have remaining samples, fill from highest SNR
    if selected.len() < MAX_TEMPLATES {
        let mut remaining: Vec<&EnrollmentSample> = samples
            .iter()
            .filter(|s| !selected.iter().any(|sel| sel.id == s.id))
            .collect();
        remaining.sort_by(|a, b| b.snr.total_cmp(&a.snr));
        for s in remaining.into_iter().take(MAX_TEMPLATES - selected.len()) {
            selected.push(s);
        }
    }

    selected
}

/// Compiles a set of samples into a Rustpotter `.rpw` personal reference model file.
pub fn compile_reference(
    name: &str,
    samples: &[EnrollmentSample],
    output_path: &Path,
) -> Result<WakewordCompileResult> {
    ensure!(
        (MIN_STEPS..=100).contains(&samples.len()),
        "Requires at least {} samples",
        MIN_STEPS
    );

    let templates = select_best_templates(samples);
    ensure!(
        (MIN_STEPS..=MAX_TEMPLATES).contains(&templates.len()),
        "Selected templates count {} outside allowed 3..=8 range",
        templates.len()
    );

    let mut buffers = HashMap::new();
    for (idx, sample) in templates.iter().enumerate() {
        let wav_bytes = encode_wav_pcm16(&sample.pcm)?;
        buffers.insert(format!("sample-{idx:02}"), wav_bytes);
    }

    // Build Rustpotter WakewordRef with 16 MFCC filters
    let reference = WakewordRef::new_from_sample_buffers(
        name.to_lowercase(),
        Some(0.48),
        Some(0.32),
        buffers,
        16,
    )
    .map_err(anyhow::Error::msg)?;

    reference
        .validate_personal_reference()
        .map_err(anyhow::Error::msg)?;

    let bytes = reference.save_to_buffer().map_err(anyhow::Error::msg)?;

    // Atomic file write: write to .tmp, then backup existing, then rename
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = output_path.with_extension("rpw.tmp");
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let mut file = options
            .open(&tmp_path)
            .context("Cannot open temporary model file for writing")?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }

    // Backup previous model if exists
    if output_path.exists() {
        let prev_path = output_path.with_extension("rpw.prev");
        let _ = std::fs::copy(output_path, prev_path);
    }

    // Atomic rename
    std::fs::rename(&tmp_path, output_path)?;

    info!(
        name = %name,
        templates_used = templates.len(),
        total_samples = samples.len(),
        bytes = bytes.len(),
        path = ?output_path,
        "Successfully compiled and saved new wake-word model"
    );

    Ok(WakewordCompileResult {
        keyword_name: name.to_string(),
        total_samples_recorded: samples.len(),
        templates_used: templates.len(),
        model_size_bytes: bytes.len(),
        output_path: output_path.to_string_lossy().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_synthetic_keyword(freq_hz: f32, duration_secs: f32) -> Vec<i16> {
        let total_samples = (SAMPLE_RATE as f32 * duration_secs) as usize;
        let mut pcm = Vec::with_capacity(total_samples);
        for i in 0..total_samples {
            // Envelope: ramp up, sustain, ramp down
            let progress = i as f32 / total_samples as f32;
            let envelope = if progress < 0.15 {
                progress / 0.15
            } else if progress > 0.85 {
                (1.0 - progress) / 0.15
            } else {
                1.0
            };
            let sample = (envelope
                * 12000.0
                * (2.0 * std::f32::consts::PI * freq_hz * (i as f32 / 16000.0)).sin())
                as i16;
            pcm.push(sample);
        }
        pcm
    }

    #[test]
    fn test_encode_decode_wav_roundtrip() {
        let original: Vec<i16> = (0..16000).map(|i| (i % 2000) as i16).collect();
        let wav = encode_wav_pcm16(&original).expect("encode");
        let decoded = decode_wav_pcm16(&wav).expect("decode");
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_trim_enrollment_with_silence_padding() {
        // 500ms silence + 600ms tone + 500ms silence
        let mut full = vec![0i16; 8000];
        let tone = generate_synthetic_keyword(440.0, 0.6);
        full.extend_from_slice(&tone);
        full.extend(vec![0i16; 8000]);

        let trimmed = trim_enrollment(&full).expect("trim");
        // Trimmed should be around tone length + margin (~0.8s = ~12800 samples)
        assert!(trimmed.len() >= 4800);
        assert!(trimmed.len() < full.len());
    }

    #[test]
    fn test_session_lifecycle_and_template_downselection() {
        let mut session = EnrollmentSession::new("testword".to_string());
        assert_eq!(session.can_compile, false);

        // Add 12 synthetic samples across different phases
        for step in 1..=12 {
            let mut audio = vec![0i16; 2000];
            audio.extend(generate_synthetic_keyword(300.0 + (step as f32 * 50.0), 0.5));
            audio.extend(vec![0i16; 2000]);
            let res = session.add_pcm_sample(&audio);
            assert!(res.is_ok(), "Sample {step} should be accepted: {:?}", res.err());
        }

        assert_eq!(session.samples.len(), 12);
        assert_eq!(session.can_compile, true);

        // Template down-selection must select at most 8 templates
        let selected = select_best_templates(&session.samples);
        assert!(selected.len() >= 3 && selected.len() <= 8);

        // Test compiling model
        let temp_dir = std::env::temp_dir();
        let model_path = temp_dir.join(format!("test_model_{}.rpw", uuid::Uuid::new_v4()));
        let compile_res = session.compile_model(&model_path).expect("compile");
        assert_eq!(compile_res.templates_used, selected.len());
        assert!(model_path.exists());
        assert!(compile_res.model_size_bytes > 0);

        // Verify model can be loaded by Rustpotter
        let loaded = WakewordRef::load_from_file(model_path.to_str().unwrap());
        assert!(loaded.is_ok());

        // Cleanup
        let _ = std::fs::remove_file(model_path);
    }
}
