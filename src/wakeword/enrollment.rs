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
pub const MAX_SAMPLES: usize = 48_000; // 3000 ms (matches rustpotter MfccWavFileExtractor)
pub const TARGET_STEPS: usize = 30;
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

    /// Cancels any active hardware recording, dropping the sender and resetting state.
    pub fn cancel(&self) {
        if let Ok(mut guard) = self.state.lock() {
            *guard = None;
        }
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
            if req.collected.len() % 8000 < 256 {
                tracing::info!(
                    collected = req.collected.len(),
                    target = req.target_samples,
                    "HardwareRecorder: accumulating mic samples"
                );
            }
            if req.collected.len() >= req.target_samples {
                tracing::info!(
                    collected = req.collected.len(),
                    "HardwareRecorder: reached target samples, sending"
                );
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
        tracing::info!(
            duration_ms = duration.as_millis(),
            target_samples,
            "HardwareRecorder: record() initiated"
        );
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
        match tokio::time::timeout(duration + Duration::from_secs(5), rx).await {
            Ok(Ok(samples)) => {
                tracing::info!(
                    samples_len = samples.len(),
                    "HardwareRecorder: record() completed successfully"
                );
                Ok(samples)
            }
            Ok(Err(_)) => bail!("Hardware recording cancelled or channel closed"),
            Err(_) => {
                let mut guard = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;
                let collected = guard.as_ref().map(|r| r.collected.len()).unwrap_or(0);
                tracing::error!(
                    collected,
                    target_samples,
                    "HardwareRecorder: record() timed out"
                );
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
            1..=10 => (1, "Ближняя зона (0.5 м, разная тональность)"),
            11..=20 => (2, "Средняя дистанция (2 м, обычная речь)"),
            _ => (3, "Дальняя зона (4–5 м, громкий призыв)"),
        }
    }

    pub fn analyze_signature(&self) -> SignatureAnalysisReport {
        analyze_samples(&self.samples)
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SampleQualityReport {
    pub id: usize,
    pub phase: u8,
    pub phase_name: String,
    pub duration_ms: u32,
    pub peak_rms: f32,
    pub snr: f32,
    pub centrality_score: f32,
    pub is_outlier: bool,
    pub outlier_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignatureAnalysisReport {
    pub overall_consistency: f32,
    pub near_consistency: f32,
    pub mid_consistency: f32,
    pub far_consistency: f32,
    pub total_samples: usize,
    pub valid_samples: usize,
    pub outlier_count: usize,
    pub similarity_matrix: Vec<Vec<f32>>,
    pub samples: Vec<SampleQualityReport>,
    pub selected_template_ids: Vec<usize>,
    pub recommendation: String,
    pub can_compile: bool,
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

/// Computes DC-removed 10 ms frame RMS energies over a PCM16 slice.
fn frame_rms_10ms(samples: &[i16]) -> Vec<f64> {
    if samples.is_empty() {
        return Vec::new();
    }
    let dc_mean = samples.iter().map(|&v| f64::from(v)).sum::<f64>() / samples.len() as f64;
    samples
        .chunks(160)
        .map(|frame| {
            (frame
                .iter()
                .map(|&v| (f64::from(v) - dc_mean).powi(2))
                .sum::<f64>()
                / frame.len() as f64)
                .sqrt()
        })
        .collect()
}

/// Applies a 5-frame (50 ms) sliding median filter to suppress isolated 10-20 ms clicks and rustles.
fn median_filter_5(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let lo = i.saturating_sub(2);
        let hi = (i + 2).min(n - 1);
        let mut win = [0.0f64; 5];
        let count = hi - lo + 1;
        win[..count].copy_from_slice(&values[lo..=hi]);
        win[..count].sort_by(f64::total_cmp);
        out.push(win[count / 2]);
    }
    out
}

fn span_from_threshold(
    rms: &[f64],
    peak_index: usize,
    threshold: f64,
    max_gap: usize,
    pad_pre: usize,
    pad_post: usize,
    total_samples: usize,
) -> (usize, usize) {
    let mut first = peak_index;
    for i in (0..peak_index).rev() {
        if rms[i] >= threshold {
            first = i;
        }
        if first.saturating_sub(i) >= max_gap {
            break;
        }
    }

    let mut last = peak_index;
    for (i, value) in rms.iter().enumerate().skip(peak_index + 1) {
        if *value >= threshold {
            last = i;
        }
        if i.saturating_sub(last) >= max_gap {
            break;
        }
    }

    let start = first.saturating_sub(pad_pre) * 160;
    let end = ((last + 1 + pad_post) * 160).min(total_samples);
    (start, end)
}

/// Computes speech boundaries `(start, end)` in sample indices using DC-free, click-filtered RMS energy.
fn trim_enrollment_bounds(samples: &[i16]) -> Result<(usize, usize)> {
    ensure!(!samples.is_empty(), "Empty audio buffer");

    let raw_rms = frame_rms_10ms(samples);
    ensure!(!raw_rms.is_empty(), "Audio too short for analysis");
    let rms = median_filter_5(&raw_rms);

    let mut sorted = rms.clone();
    sorted.sort_by(f64::total_cmp);

    let noise = sorted[sorted.len() / 10];
    let peak = *sorted.last().context("Empty recording")?;

    ensure!(
        peak >= 1.0 && peak >= noise * 1.5,
        "В записи не обнаружено отчётливого голоса (слишком тихо или фоновый шум)"
    );

    let peak_index = rms
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .context("Empty recording")?;

    let primary_thr = (peak * 0.30).max(noise * 3.5).min(peak * 0.65);
    let (mut start, mut end) =
        span_from_threshold(&rms, peak_index, primary_thr, 36, 10, 12, samples.len());

    // If low-level rustling/breathing bridged beyond 3.0s, tighten around the vocal core before failing.
    if end.saturating_sub(start) > MAX_SAMPLES {
        for &(pct_peak, noise_mult, max_cap, max_gap, pad_pre, pad_post) in &[
            (0.36_f64, 3.5_f64, 0.65_f64, 28_usize, 8_usize, 10_usize),
            (0.42, 4.0, 0.65, 20, 6, 8),
        ] {
            let thr = (peak * pct_peak)
                .max(noise * noise_mult)
                .min(peak * max_cap);
            let (cand_start, cand_end) = span_from_threshold(
                &rms,
                peak_index,
                thr,
                max_gap,
                pad_pre,
                pad_post,
                samples.len(),
            );
            start = cand_start;
            end = cand_end;
            if end.saturating_sub(start) <= MAX_SAMPLES {
                break;
            }
        }
    }

    let len = end.saturating_sub(start);
    tracing::debug!(
        raw_samples = samples.len(),
        ac_peak_rms = peak as f32,
        ac_noise_p10 = noise as f32,
        trimmed_start_ms = (start as f64 / 16.0).round() as u32,
        trimmed_end_ms = (end as f64 / 16.0).round() as u32,
        trimmed_ms = (len as f64 / 16.0).round() as u32,
        "Enrollment trim computed"
    );

    ensure!(
        len >= MIN_SAMPLES,
        "Произнесённое слово слишком короткое ({:.1} с). Говорите чётко не менее 0.3 с.",
        len as f64 / 16000.0
    );
    ensure!(
        len <= MAX_SAMPLES,
        "Речь или фоновый шум заняли {:.1} с (допустимо до 3.0 с). Говорите короче и соблюдайте тишину после фразы.",
        len as f64 / 16000.0
    );

    Ok((start, end))
}

/// Trims leading and trailing silence using dynamic RMS energy threshold.
#[cfg(test)]
pub fn trim_enrollment(samples: &[i16]) -> Result<Vec<i16>> {
    let (start, end) = trim_enrollment_bounds(samples)?;
    Ok(samples[start..end].to_vec())
}

/// Evaluates a raw recording, trims silence, calculates metrics, and returns an `EnrollmentSample`.
pub fn process_sample(
    raw_pcm: &[i16],
    id: usize,
    phase: u8,
    phase_name: &str,
) -> Result<EnrollmentSample> {
    ensure!(raw_pcm.len() >= 1600, "Запись слишком короткая для анализа");

    // Calculate overall peak and noise floor
    let mut sum_sq = 0.0f64;
    let mut max_abs = 0i32;
    for &s in raw_pcm {
        let abs = (s as i32).abs();
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

    let (start, end) = trim_enrollment_bounds(raw_pcm)?;
    let trimmed = raw_pcm[start..end].to_vec();
    let duration_ms = (trimmed.len() as f64 / 16.0).round() as u32;

    // Quality check SNR from quiet background frames outside the trimmed speech envelope
    let mut trimmed_sum_sq = 0.0f64;
    for &s in &trimmed {
        trimmed_sum_sq += f64::from(s).powi(2);
    }
    let trimmed_rms = (trimmed_sum_sq / trimmed.len() as f64).sqrt() as f32;
    let mut outside_rms = frame_rms_10ms(&raw_pcm[..start]);
    outside_rms.extend(frame_rms_10ms(&raw_pcm[end..]));
    let noise_rms = if outside_rms.len() >= 5 {
        outside_rms.sort_by(f64::total_cmp);
        (outside_rms[outside_rms.len() / 2] as f32).max(1.0)
    } else {
        let mut all_rms = frame_rms_10ms(raw_pcm);
        all_rms.sort_by(f64::total_cmp);
        all_rms
            .get(all_rms.len() / 20)
            .copied()
            .unwrap_or(f64::from(total_rms))
            .max(1.0) as f32
    };
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

/// Extracts 16 MFCC filter coefficients for a recorded PCM sample.
pub fn extract_sample_mfcc(sample: &EnrollmentSample) -> Result<Vec<Vec<f32>>> {
    let wav_bytes = encode_wav_pcm16(&sample.pcm)?;
    let mut rms = 0.0f32;
    let features = rustpotter::mfcc::MfccWavFileExtractor::compute_mfccs(
        std::io::BufReader::new(wav_bytes.as_slice()),
        &mut rms,
        16,
    )
    .map_err(anyhow::Error::msg)?;
    Ok(features)
}

/// Evaluates acoustic consistency across recorded samples using dynamic time warping (DTW)
/// on 16 MFCC features. Generates NxN pairwise similarity matrix, per-sample centrality scores,
/// detects noisy/mismatched outliers, and selects 7 optimal centroids.
pub fn analyze_samples(samples: &[EnrollmentSample]) -> SignatureAnalysisReport {
    let n = samples.len();
    if n < MIN_STEPS {
        let sample_reports = samples
            .iter()
            .map(|s| SampleQualityReport {
                id: s.id,
                phase: s.phase,
                phase_name: s.phase_name.clone(),
                duration_ms: s.duration_ms,
                peak_rms: s.peak_rms,
                snr: s.snr,
                centrality_score: 1.0,
                is_outlier: false,
                outlier_reason: None,
            })
            .collect();

        return SignatureAnalysisReport {
            overall_consistency: 0.0,
            near_consistency: 0.0,
            mid_consistency: 0.0,
            far_consistency: 0.0,
            total_samples: n,
            valid_samples: n,
            outlier_count: 0,
            similarity_matrix: vec![vec![1.0; n]; n],
            samples: sample_reports,
            selected_template_ids: samples.iter().map(|s| s.id).collect(),
            recommendation: format!(
                "Записано {} из {} сэмплов. Запишите минимум {} для анализа сигнатуры.",
                n, TARGET_STEPS, MIN_STEPS
            ),
            can_compile: false,
        };
    }

    // 1. Extract MFCC-16 feature vectors for all samples
    let mut features_list: Vec<Option<Vec<Vec<f32>>>> = Vec::with_capacity(n);
    for s in samples {
        match extract_sample_mfcc(s) {
            Ok(f) => features_list.push(Some(f)),
            Err(e) => {
                tracing::warn!(sample_id = s.id, "MFCC extraction failed: {e}");
                features_list.push(None);
            }
        }
    }

    // 2. Compute pairwise similarity matrix S[i][j] via DTW with window band 5
    let mut matrix = vec![vec![0.0f32; n]; n];
    for i in 0..n {
        matrix[i][i] = 1.0;
        for j in (i + 1)..n {
            let sim = match (&features_list[i], &features_list[j]) {
                (Some(feat_a), Some(feat_b)) => {
                    let a_refs: Vec<&[f32]> = feat_a.iter().map(|r| r.as_slice()).collect();
                    let b_refs: Vec<&[f32]> = feat_b.iter().map(|r| r.as_slice()).collect();
                    let mut dtw = rustpotter::mfcc::Dtw::new(
                        rustpotter::mfcc::MfccComparator::calculate_distance,
                    );
                    let band = 5usize
                        .max(a_refs.len().abs_diff(b_refs.len()) + 5)
                        .min(usize::from(u16::MAX)) as u16;
                    let cost = dtw.compute_optimal_path_with_window(&a_refs, &b_refs, band);
                    let norm_cost = cost / (a_refs.len() + b_refs.len()) as f32;
                    let val = (1.0 - norm_cost).clamp(0.0, 1.0);
                    (val * 100.0).round() / 100.0
                }
                _ => 0.0,
            };
            matrix[i][j] = sim;
            matrix[j][i] = sim;
        }
    }

    // 3. Centrality score: mean similarity to all other samples
    let mut centralities = vec![0.0f32; n];
    for (i, row) in matrix.iter().enumerate().take(n) {
        let mut sum = 0.0f32;
        let mut count = 0usize;
        for (j, &sim) in row.iter().enumerate().take(n) {
            if i != j {
                sum += sim;
                count += 1;
            }
        }
        centralities[i] = if count > 0 {
            ((sum / count as f32) * 1000.0).round() / 1000.0
        } else {
            1.0
        };
    }

    // 4. Overall and per-zone consistency metrics
    let mut sum_all = 0.0f32;
    let mut count_all = 0usize;
    let mut sum_near = 0.0f32;
    let mut count_near = 0usize;
    let mut sum_mid = 0.0f32;
    let mut count_mid = 0usize;
    let mut sum_far = 0.0f32;
    let mut count_far = 0usize;

    for i in 0..n {
        for j in (i + 1)..n {
            let val = matrix[i][j];
            sum_all += val;
            count_all += 1;

            if samples[i].phase == 1 && samples[j].phase == 1 {
                sum_near += val;
                count_near += 1;
            } else if samples[i].phase == 2 && samples[j].phase == 2 {
                sum_mid += val;
                count_mid += 1;
            } else if samples[i].phase == 3 && samples[j].phase == 3 {
                sum_far += val;
                count_far += 1;
            }
        }
    }

    let overall_consistency = if count_all > 0 {
        ((sum_all / count_all as f32) * 10000.0).round() / 100.0
    } else {
        0.0
    };
    let near_consistency = if count_near > 0 {
        ((sum_near / count_near as f32) * 10000.0).round() / 100.0
    } else {
        overall_consistency
    };
    let mid_consistency = if count_mid > 0 {
        ((sum_mid / count_mid as f32) * 10000.0).round() / 100.0
    } else {
        overall_consistency
    };
    let far_consistency = if count_far > 0 {
        ((sum_far / count_far as f32) * 10000.0).round() / 100.0
    } else {
        overall_consistency
    };

    // 5. Outlier Detection: MFCC failure, severe noise (< 6 dB), duration drift, or acoustic isolation
    let mut valid_durations: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(i, s)| features_list[*i].is_some() && s.snr >= 6.0)
        .map(|(_, s)| s.duration_ms)
        .collect();
    valid_durations.sort_unstable();
    let median_duration_ms = if valid_durations.len() >= 5 {
        Some(valid_durations[valid_durations.len() / 2])
    } else {
        None
    };

    let mut is_outlier_vec = vec![false; n];
    let mut outlier_reasons: Vec<Option<String>> = vec![None; n];

    for (i, s) in samples.iter().enumerate() {
        if features_list[i].is_none() {
            is_outlier_vec[i] = true;
            outlier_reasons[i] = Some("Ошибка извлечения акустических признаков MFCC".to_string());
        } else if s.snr < 6.0 {
            is_outlier_vec[i] = true;
            outlier_reasons[i] = Some(format!(
                "Слишком высокий фоновый шум: SNR {:.1} дБ (< 6 дБ)",
                s.snr
            ));
        } else if let Some(med) = median_duration_ms {
            let min_dur = ((med as f32) * 0.55).round() as u32;
            let max_dur = ((med as f32) * 1.50).round() as u32;
            if s.duration_ms < min_dur || s.duration_ms > max_dur {
                is_outlier_vec[i] = true;
                outlier_reasons[i] = Some(format!(
                    "Длительность сильно отличается от остальных сэмплов ({} мс при медиане {} мс)",
                    s.duration_ms, med
                ));
            }
        }
    }

    let remaining_valid = is_outlier_vec.iter().filter(|&&flagged| !flagged).count();
    if remaining_valid >= 5 {
        let mut valid_max_peers: Vec<f32> = Vec::with_capacity(remaining_valid);
        let mut valid_cents: Vec<f32> = Vec::with_capacity(remaining_valid);
        let mut peer_max_per_idx = vec![0.0f32; n];
        for i in 0..n {
            if is_outlier_vec[i] {
                continue;
            }
            let max_peer_sim = matrix[i]
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i && !is_outlier_vec[*j])
                .map(|(_, &v)| v)
                .fold(0.0f32, f32::max);
            peer_max_per_idx[i] = max_peer_sim;
            valid_max_peers.push(max_peer_sim);
            valid_cents.push(centralities[i]);
        }
        valid_max_peers.sort_by(f32::total_cmp);
        valid_cents.sort_by(f32::total_cmp);
        let med_max_peer = valid_max_peers[valid_max_peers.len() / 2];
        let med_cent = valid_cents[valid_cents.len() / 2];
        let min_allowed_peer = (med_max_peer - 0.25).max(0.35);
        let min_allowed_cent = med_cent * 0.65;

        for i in 0..n {
            if is_outlier_vec[i] {
                continue;
            }
            let max_peer_sim = peer_max_per_idx[i];
            if max_peer_sim < min_allowed_peer || centralities[i] < min_allowed_cent {
                is_outlier_vec[i] = true;
                outlier_reasons[i] = Some(format!(
                    "Акустическая сигнатура не совпадает с остальными дублями (макс. {:.2}, центр. {:.2})",
                    max_peer_sim, centralities[i]
                ));
            }
        }
    }

    let mut sample_reports = Vec::with_capacity(n);
    let mut outlier_count = 0usize;

    for (i, s) in samples.iter().enumerate() {
        let is_outlier = is_outlier_vec[i];
        if is_outlier {
            outlier_count += 1;
        }

        sample_reports.push(SampleQualityReport {
            id: s.id,
            phase: s.phase,
            phase_name: s.phase_name.clone(),
            duration_ms: s.duration_ms,
            peak_rms: s.peak_rms,
            snr: s.snr,
            centrality_score: centralities[i],
            is_outlier,
            outlier_reason: outlier_reasons[i].clone(),
        });
    }

    // 6. Centrality-constrained diversity selection (Target: up to 8 templates across 3 zones)
    let mut non_outliers: Vec<(usize, &EnrollmentSample, f32)> = sample_reports
        .iter()
        .zip(samples.iter())
        .filter(|(r, _)| !r.is_outlier)
        .map(|(r, s)| (s.id, s, r.centrality_score))
        .collect();

    // If too few non-outliers, fall back to all valid-MFCC samples (or all samples if needed)
    if non_outliers.len() < MIN_STEPS {
        non_outliers = sample_reports
            .iter()
            .zip(samples.iter())
            .enumerate()
            .filter(|(i, _)| features_list[*i].is_some())
            .map(|(_, (r, s))| (s.id, s, r.centrality_score))
            .collect();
    }
    if non_outliers.len() < MIN_STEPS {
        non_outliers = sample_reports
            .iter()
            .zip(samples.iter())
            .map(|(r, s)| (s.id, s, r.centrality_score))
            .collect();
    }

    // Build index mapping sample id → matrix row
    let id_to_idx: std::collections::HashMap<usize, usize> =
        samples.iter().enumerate().map(|(i, s)| (s.id, i)).collect();

    let target_total = MAX_TEMPLATES.min(non_outliers.len()).max(MIN_STEPS);

    let mut selected_ids: Vec<usize> = Vec::with_capacity(target_total);
    {
        // Phase-aware seeding: seed one most-central sample from each phase first
        for phase in [1u8, 2, 3] {
            if selected_ids.len() >= target_total {
                break;
            }
            if let Some((id, _c)) = non_outliers
                .iter()
                .filter(|(_, s, _)| s.phase == phase)
                .map(|(id, _, c)| (*id, *c))
                .max_by(|a, b| a.1.total_cmp(&b.1))
            {
                if !selected_ids.contains(&id) {
                    selected_ids.push(id);
                }
            }
        }

        let mean_candidate_c =
            non_outliers.iter().map(|(_, _, c)| *c).sum::<f32>() / non_outliers.len().max(1) as f32;
        let min_spread_c = mean_candidate_c * 0.70;

        // Centrality-guarded greedy spread: maximize acoustic span without selecting isolated quirks
        while selected_ids.len() < target_total {
            let pick_among = |require_floor: bool| -> Option<usize> {
                non_outliers
                    .iter()
                    .filter(|(id, _, c)| {
                        !selected_ids.contains(id) && (!require_floor || *c >= min_spread_c)
                    })
                    .map(|(id, _, c)| {
                        let min_sim = selected_ids
                            .iter()
                            .filter_map(|sel_id| {
                                let ri = id_to_idx.get(id)?;
                                let rj = id_to_idx.get(sel_id)?;
                                Some(matrix[*ri][*rj])
                            })
                            .fold(f32::INFINITY, f32::min);
                        (*id, if min_sim.is_infinite() { 0.0 } else { min_sim }, *c)
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.2.total_cmp(&a.2)))
                    .map(|(id, _, _)| id)
            };

            match pick_among(true).or_else(|| pick_among(false)) {
                Some(id) => selected_ids.push(id),
                None => break,
            }
        }
    }

    selected_ids.sort();

    let can_compile = (MIN_STEPS..=MAX_TEMPLATES).contains(&selected_ids.len());

    let recommendation = if overall_consistency >= 75.0 && outlier_count == 0 {
        format!(
            "Отличная акустическая сигнатура (консистентность {:.1}%). Модель из {} эталонов готова к компиляции.",
            overall_consistency, selected_ids.len()
        )
    } else if overall_consistency >= 40.0 {
        if outlier_count > 0 {
            format!(
                "Хороший акустический охват ({:.1}%). Выбросов по шуму: {}. Модель готова к компиляции.",
                overall_consistency, outlier_count
            )
        } else {
            format!(
                "Хорошее акустическое разнообразие ({:.1}%). Модель готова к компиляции.",
                overall_consistency
            )
        }
    } else if outlier_count > 0 {
        format!(
            "Акустический охват ({:.1}%). Обнаружено шумных сэмплов: {}. Рекомендуется проверить их перед активацией.",
            overall_consistency, outlier_count
        )
    } else {
        format!(
            "Акустический охват ({:.1}%). Разнообразие интонаций и дистанций учтено. Модель готова к компиляции.",
            overall_consistency
        )
    };

    SignatureAnalysisReport {
        overall_consistency,
        near_consistency,
        mid_consistency,
        far_consistency,
        total_samples: n,
        valid_samples: n.saturating_sub(outlier_count),
        outlier_count,
        similarity_matrix: matrix,
        samples: sample_reports,
        selected_template_ids: selected_ids,
        recommendation,
        can_compile,
    }
}

/// Selects up to `MAX_TEMPLATES` (8) best templates using the signature analysis centroids.
pub fn select_best_templates(samples: &[EnrollmentSample]) -> Vec<&EnrollmentSample> {
    if samples.len() <= MAX_TEMPLATES && samples.len() >= MIN_STEPS {
        return samples.iter().collect();
    }

    let report = analyze_samples(samples);
    let mut selected = Vec::new();
    for &id in &report.selected_template_ids {
        if let Some(s) = samples.iter().find(|s| s.id == id) {
            selected.push(s);
        }
    }

    if selected.len() >= MIN_STEPS {
        selected
    } else {
        // Fallback: highest SNR
        let mut sorted: Vec<&EnrollmentSample> = samples.iter().collect();
        sorted.sort_by(|a, b| b.snr.total_cmp(&a.snr));
        sorted.truncate(MAX_TEMPLATES);
        sorted
    }
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

    // Build Rustpotter WakewordRef with 16 MFCC filters (runtime detector supplies thresholds)
    let reference =
        WakewordRef::new_from_sample_buffers(name.to_lowercase(), None, None, buffers, 16)
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
    use rustpotter::WakewordLoad;

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
    fn test_trim_enrollment_preserves_compound_word_pause() {
        // 400ms silence + 600ms word1 + 250ms inter-word pause + 700ms word2 + 400ms silence
        let mut full = vec![0i16; 6400]; // 400ms silence
        full.extend_from_slice(&generate_synthetic_keyword(440.0, 0.6)); // word 1
        full.extend(vec![0i16; 4000]); // 250ms pause (4000 samples)
        full.extend_from_slice(&generate_synthetic_keyword(520.0, 0.7)); // word 2
        full.extend(vec![0i16; 6400]); // 400ms silence

        let trimmed = trim_enrollment(&full).expect("trim");
        // Both word1, the 250ms pause, and word2 must be fully retained
        // Duration should be at least (0.6 + 0.25 + 0.7) * 16000 = 24800 samples
        assert!(
            trimmed.len() >= 24800,
            "Trimmed length {} too short, compound word cut off!",
            trimmed.len()
        );
        assert!(trimmed.len() < full.len());
    }

    #[test]
    fn test_session_lifecycle_and_template_downselection() {
        let mut session = EnrollmentSession::new("testword".to_string());
        assert!(!session.can_compile);

        // Add 12 synthetic samples across different phases
        for step in 1..=12 {
            let mut audio = vec![0i16; 2000];
            audio.extend(generate_synthetic_keyword(
                300.0 + (step as f32 * 50.0),
                0.5,
            ));
            audio.extend(vec![0i16; 2000]);
            let res = session.add_pcm_sample(&audio);
            assert!(
                res.is_ok(),
                "Sample {step} should be accepted: {:?}",
                res.err()
            );
        }

        assert_eq!(session.samples.len(), 12);
        assert!(session.can_compile);

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

    #[test]
    fn test_signature_analysis_matrix_and_outliers() {
        let mut session = EnrollmentSession::new("shtuchka".to_string());

        // Add 6 similar samples (440Hz base tone)
        for _ in 1..=6 {
            let mut audio = vec![0i16; 2000];
            audio.extend(generate_synthetic_keyword(440.0, 0.5));
            audio.extend(vec![0i16; 2000]);
            session.add_pcm_sample(&audio).expect("sample");
        }

        // Add 1 outlier sample (completely mismatched 3500Hz high pitch)
        let mut noise_sample = vec![0i16; 2000];
        noise_sample.extend(generate_synthetic_keyword(3500.0, 0.5));
        noise_sample.extend(vec![0i16; 2000]);
        session.add_pcm_sample(&noise_sample).expect("noise sample");

        let report = session.analyze_signature();
        assert_eq!(report.total_samples, 7);
        assert_eq!(report.similarity_matrix.len(), 7);

        // Check diagonal and symmetry
        for i in 0..7 {
            assert_eq!(report.similarity_matrix[i][i], 1.0);
            for j in 0..7 {
                assert_eq!(
                    report.similarity_matrix[i][j],
                    report.similarity_matrix[j][i]
                );
            }
        }

        // Check that sample 7 (the high frequency outlier) has much lower centrality than sample 1
        let normal_c = report.samples[0].centrality_score;
        let outlier_c = report.samples[6].centrality_score;
        assert!(
            normal_c > outlier_c,
            "Normal sample centrality {normal_c} should be > outlier centrality {outlier_c}"
        );

        // Check that selected templates do NOT include the outlier if we have enough normal ones
        assert!(
            !report.selected_template_ids.contains(&7) || report.selected_template_ids.len() == 7
        );
    }

    #[test]
    fn test_signature_analysis_30_samples_centroid_selection() {
        let mut session = EnrollmentSession::new("shtuchka".to_string());

        // Add 30 samples across the 3 distance zones (10 near, 10 mid, 10 far)
        for step in 1..=30 {
            let freq = match step {
                1..=10 => 400.0 + (step as f32 * 5.0),  // Near
                11..=20 => 450.0 + (step as f32 * 3.0), // Mid
                _ => 500.0 + (step as f32 * 2.0),       // Far
            };
            let mut audio = vec![0i16; 2000];
            audio.extend(generate_synthetic_keyword(freq, 0.5));
            audio.extend(vec![0i16; 2000]);
            session.add_pcm_sample(&audio).expect("sample");
        }

        assert_eq!(session.samples.len(), 30);
        let report = session.analyze_signature();

        assert_eq!(report.total_samples, 30);
        assert!(report.overall_consistency > 50.0);
        assert!(report.near_consistency > 50.0);
        assert!(report.mid_consistency > 50.0);
        assert!(report.far_consistency > 50.0);

        // Diversity-first selection: target is MAX_TEMPLATES (8), one seed per phase + greedy spread.
        // We only assert the range, not the exact count, because greedy spread may stop early
        // if candidates are exhausted (synthetic data may not have all phases populated).
        let n_selected = report.selected_template_ids.len();
        assert!(
            (MIN_STEPS..=MAX_TEMPLATES).contains(&n_selected),
            "Selected template count {n_selected} should be in [{MIN_STEPS}, {MAX_TEMPLATES}]"
        );

        // All three distance zones should be represented (at least 1 seed per phase)
        let phase1_count = report
            .selected_template_ids
            .iter()
            .filter(|&&id| (1..=10).contains(&id))
            .count();
        let phase2_count = report
            .selected_template_ids
            .iter()
            .filter(|&&id| (11..=20).contains(&id))
            .count();
        let phase3_count = report
            .selected_template_ids
            .iter()
            .filter(|&&id| (21..=30).contains(&id))
            .count();

        assert!(phase1_count >= 1, "Near zone must have at least 1 template");
        assert!(phase2_count >= 1, "Mid zone must have at least 1 template");
        assert!(phase3_count >= 1, "Far zone must have at least 1 template");
        assert!(report.can_compile);
    }

    #[test]
    fn test_process_sample_i16_min_no_panic_and_detects_clipping() {
        let mut pcm = vec![1000i16; 3200]; // 200 ms
        pcm[500] = i16::MIN; // -32768
        let res = process_sample(&pcm, 1, 1, "near");
        assert!(res.is_err(), "i16::MIN must be rejected as clipping");
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("клиппингом"),
            "Expected clipping error, got: {err_msg}"
        );
    }

    #[test]
    fn test_process_sample_rejects_stationary_background_noise_by_real_snr() {
        // 3.5 seconds of broadband-like multi-tone stationary noise (no distinct clean silence floor)
        let pcm: Vec<i16> = (0..56_000)
            .map(|i| {
                let t = i as f32 / 16000.0;
                ((t * 310.0).sin() * 1400.0
                    + (t * 733.0).sin() * 1400.0
                    + (t * 1411.0).sin() * 1200.0) as i16
            })
            .collect();
        // Either trimming rejects uniform floor (peak < 1.5*noise), or SNR is below 6 dB
        if let Ok(sample) = process_sample(&pcm, 1, 1, "near") {
            assert!(
                sample.snr < 6.0,
                "stationary continuous noise must not get artificial >6dB SNR, got {}",
                sample.snr
            );
        }
    }

    #[test]
    fn test_analyze_samples_flags_duration_and_isolated_outliers() {
        let mut session = EnrollmentSession::new("shtuchka".to_string());
        for _ in 0..8 {
            let mut audio = vec![0i16; 2400];
            audio.extend(generate_synthetic_keyword(440.0, 1.0));
            audio.extend(vec![0i16; 2400]);
            session.add_pcm_sample(&audio).expect("normal sample");
        }
        // Too short duration outlier (0.35s vs 1.0s median -> id=9)
        let mut short_audio = vec![0i16; 2400];
        short_audio.extend(generate_synthetic_keyword(440.0, 0.35));
        short_audio.extend(vec![0i16; 2400]);
        session.add_pcm_sample(&short_audio).expect("short outlier");

        // Acoustic pitch outlier matching normal duration (id=10)
        let mut weird_audio = vec![0i16; 2400];
        weird_audio.extend(generate_synthetic_keyword(3600.0, 1.0));
        weird_audio.extend(vec![0i16; 2400]);
        session.add_pcm_sample(&weird_audio).expect("weird outlier");

        let report = session.analyze_signature();
        let short_report = report.samples.iter().find(|s| s.id == 9).unwrap();
        let weird_report = report.samples.iter().find(|s| s.id == 10).unwrap();
        assert!(
            short_report.is_outlier,
            "short duration fragment must be flagged"
        );
        assert!(
            weird_report.is_outlier,
            "isolated signature must be flagged"
        );
        assert!(!report.selected_template_ids.contains(&9));
        assert!(!report.selected_template_ids.contains(&10));
    }

    #[test]
    fn test_trim_enrollment_ignores_laptop_clicks_and_dc_offset() {
        // 3.8 s buffer with +500 DC bias, one 1.0 s spoken phrase, and periodic 20 ms key clicks
        let mut pcm = vec![500i16; 60_800];
        let word = generate_synthetic_keyword(480.0, 1.0);
        for (idx, s) in word.into_iter().enumerate() {
            pcm[8_000 + idx] = s.saturating_add(500);
        }
        // Inject 20 ms (320 samples) high-amplitude laptop key clicks every 250 ms across the buffer
        for click_start in (400..59_000).step_by(4_000) {
            for j in 0..320 {
                let sign = if j % 2 == 0 { 1 } else { -1 };
                pcm[click_start + j] = 9_000 * sign + 500;
            }
        }

        let trimmed =
            trim_enrollment(&pcm).expect("key clicks and DC offset must not reject speech");
        assert!(
            trimmed.len() >= 14_000 && trimmed.len() <= 24_000,
            "expected ~1.0-1.4s trimmed word, got {} samples ({:.2}s)",
            trimmed.len(),
            trimmed.len() as f32 / 16000.0
        );
    }

    #[test]
    fn test_trim_enrollment_still_rejects_sustained_speech_over_3s() {
        let mut pcm = vec![0i16; 3200];
        pcm.extend(generate_synthetic_keyword(440.0, 3.5));
        pcm.extend(vec![0i16; 3200]);

        let err = trim_enrollment(&pcm)
            .expect_err("3.5s continuous utterance must still exceed 3.0s limit")
            .to_string();
        assert!(
            err.contains("3.0 с"),
            "expected max 3.0s message, got: {err}"
        );
    }
}
