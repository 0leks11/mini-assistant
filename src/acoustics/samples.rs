//! Control samples library, strict WAV PCM16 16kHz validation,
//! audio metrics (clipping detection, SNR, RMS), and dataset split separation.

use anyhow::{bail, ensure, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use num_complex::Complex32;
use rustfft::FftPlanner;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_SAMPLE_DURATION_SECS: u32 = 60;
pub const TARGET_SAMPLE_RATE: u32 = 16_000;
pub const TARGET_CHANNELS: u16 = 1;
pub const TARGET_BITS_PER_SAMPLE: u16 = 16;
pub const MAX_SAMPLE_BYTES: usize = 2_000_000; // ~1.92 MB PCM + headers

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSource {
    Upload,
    Recorded,
    Synthetic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleLabel {
    #[serde(rename = "positive", alias = "positive_wake_word")]
    PositiveWakeWord,
    #[serde(rename = "negative", alias = "negative_speech")]
    NegativeSpeech,
    #[serde(rename = "own_output")]
    OwnOutput,
    #[serde(rename = "ambient", alias = "ambient_noise")]
    AmbientNoise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSplit {
    Train,
    Calibration,
    HeldOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleInterval {
    pub start_ms: u32,
    pub end_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleAudioMetrics {
    pub duration_ms: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub rms_level: f32,
    pub peak_amplitude: i32,
    pub is_clipped: bool,
    pub snr_db: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleMetadata {
    pub id: String,
    pub name: String,
    pub source: SampleSource,
    pub label: SampleLabel,
    pub split: SampleSplit,
    pub interval: Option<SampleInterval>,
    pub metrics: SampleAudioMetrics,
    pub notes: Option<String>,
    pub archived: bool,
    pub created_at: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSampleDto {
    pub name: String,
    pub source: Option<SampleSource>,
    pub label: SampleLabel,
    pub split: SampleSplit,
    pub interval: Option<SampleInterval>,
    pub notes: Option<String>,
    pub wav_base64: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateSampleDto {
    pub name: Option<String>,
    pub label: Option<SampleLabel>,
    pub split: Option<SampleSplit>,
    pub interval: Option<SampleInterval>,
    pub notes: Option<String>,
    pub archived: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplesIndex {
    pub schema_version: u32,
    pub samples: HashMap<String, SampleMetadata>,
}

impl Default for SamplesIndex {
    fn default() -> Self {
        Self {
            schema_version: 1,
            samples: HashMap::new(),
        }
    }
}

/// Standard FIPS 180-4 SHA-256 implementation without external crate dependencies.
pub fn compute_sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while !(msg.len() + 8).is_multiple_of(64) {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut h_var = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h_var
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h_var = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(h_var);
    }

    format!(
        "{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]
    )
}

/// Strictly validates that the provided audio bytes conform to mono PCM16 16000Hz WAV,
/// does not exceed 60 seconds, is not truncated, and extracts audio metrics.
pub fn validate_and_analyze_wav(
    bytes: &[u8],
    interval: Option<SampleInterval>,
) -> Result<(Vec<i16>, SampleAudioMetrics)> {
    ensure!(
        bytes.len() >= 44,
        "Audio file too small to contain valid WAV header"
    );
    ensure!(
        bytes.len() <= MAX_SAMPLE_BYTES,
        "WAV exceeds maximum size limit of {} bytes (approx 60s at 16kHz PCM16)",
        MAX_SAMPLE_BYTES
    );

    let cursor = Cursor::new(bytes);
    let mut reader = WavReader::new(cursor).context("Invalid WAV header or format")?;
    let spec = reader.spec();

    ensure!(
        spec.channels == TARGET_CHANNELS,
        "Expected mono audio (1 channel), got {} channels",
        spec.channels
    );
    ensure!(
        spec.sample_rate == TARGET_SAMPLE_RATE,
        "Expected 16000 Hz sample rate, got {} Hz",
        spec.sample_rate
    );
    ensure!(
        spec.bits_per_sample == TARGET_BITS_PER_SAMPLE,
        "Expected 16 bits per sample, got {} bits",
        spec.bits_per_sample
    );
    ensure!(
        spec.sample_format == SampleFormat::Int,
        "Expected signed integer PCM format"
    );

    let mut samples: Vec<i16> = Vec::new();
    for sample_res in reader.samples::<i16>() {
        match sample_res {
            Ok(s) => samples.push(s),
            Err(e) => bail!("Truncated or corrupted WAV data: {e}"),
        }
    }

    ensure!(!samples.is_empty(), "WAV file contains no audio samples");
    let max_samples = (TARGET_SAMPLE_RATE * MAX_SAMPLE_DURATION_SECS) as usize;
    ensure!(
        samples.len() <= max_samples,
        "Audio clip duration ({:.2} s) exceeds maximum limit of {} s",
        samples.len() as f64 / TARGET_SAMPLE_RATE as f64,
        MAX_SAMPLE_DURATION_SECS
    );

    let duration_ms = ((samples.len() as f64 / TARGET_SAMPLE_RATE as f64) * 1000.0).round() as u32;

    // Metrics calculation with overflow protection on i16::MIN
    let mut max_abs = 0i32;
    let mut sum_sq = 0.0f64;
    let mut clipped_count = 0usize;

    for &s in &samples {
        let abs = (s as i32).abs(); // -32768 becomes 32768 without panic
        if abs > max_abs {
            max_abs = abs;
        }
        if s <= i16::MIN + 5 || s >= i16::MAX - 5 {
            clipped_count += 1;
        }
        sum_sq += f64::from(s).powi(2);
    }

    let is_clipped = clipped_count > 0;
    let rms_level = (sum_sq / samples.len() as f64).sqrt() as f32;

    // SNR computation
    let snr_db = if let Some(iv) = interval {
        ensure!(
            iv.start_ms < iv.end_ms,
            "Interval start_ms ({}) must be less than end_ms ({})",
            iv.start_ms,
            iv.end_ms
        );
        ensure!(
            iv.end_ms <= duration_ms + 50,
            "Interval end_ms ({}) exceeds audio duration ({} ms)",
            iv.end_ms,
            duration_ms
        );

        let start_sample = ((iv.start_ms as f64 / 1000.0) * TARGET_SAMPLE_RATE as f64) as usize;
        let end_sample =
            (((iv.end_ms as f64 / 1000.0) * TARGET_SAMPLE_RATE as f64) as usize).min(samples.len());

        let speech_samples = &samples[start_sample..end_sample];
        let speech_rms = if !speech_samples.is_empty() {
            let sp_sq: f64 = speech_samples.iter().map(|&s| f64::from(s).powi(2)).sum();
            (sp_sq / speech_samples.len() as f64).sqrt() as f32
        } else {
            rms_level
        };

        let lead_noise = &samples[..start_sample];
        let tail_noise = &samples[end_sample..];
        let noise_count = lead_noise.len() + tail_noise.len();

        let noise_rms = if noise_count >= 160 {
            let n_sq: f64 = lead_noise
                .iter()
                .chain(tail_noise.iter())
                .map(|&s| f64::from(s).powi(2))
                .sum();
            (n_sq / noise_count as f64).sqrt() as f32
        } else {
            1.0f32
        };

        (20.0 * (speech_rms / noise_rms.max(1.0)).log10()).clamp(-10.0, 60.0)
    } else {
        // Compute frame-based energy estimate (20ms frames = 320 samples)
        let frame_size = 320;
        let mut frame_energies: Vec<f32> = samples
            .chunks(frame_size)
            .map(|chunk| {
                let sq: f64 = chunk.iter().map(|&s| f64::from(s).powi(2)).sum();
                (sq / chunk.len() as f64).sqrt() as f32
            })
            .collect();
        frame_energies.sort_by(|a, b| a.total_cmp(b));

        if frame_energies.len() >= 4 {
            let noise_idx = frame_energies.len() / 5; // bottom 20%
            let signal_idx = (frame_energies.len() * 4) / 5; // top 20%
            let noise_rms = frame_energies[noise_idx].max(1.0);
            let signal_rms = frame_energies[signal_idx];
            (20.0 * (signal_rms / noise_rms).log10()).clamp(-10.0, 60.0)
        } else {
            12.0f32
        }
    };

    Ok((
        samples,
        SampleAudioMetrics {
            duration_ms,
            sample_rate: TARGET_SAMPLE_RATE,
            channels: TARGET_CHANNELS,
            bits_per_sample: TARGET_BITS_PER_SAMPLE,
            rms_level,
            peak_amplitude: max_abs,
            is_clipped,
            snr_db,
        },
    ))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SampleSpectrogramBands {
    pub sub_bass: f32,
    pub low_mid: f32,
    pub mid: f32,
    pub presence: f32,
    pub brilliance: f32,
    pub full: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleSpectrogram {
    pub sample_rate: u32,
    pub fft_size: usize,
    pub hop_ms: f32,
    pub duration_ms: u32,
    pub freq_bands_hz: Vec<u16>,
    pub band_energies_dbfs: SampleSpectrogramBands,
    pub frames_db: Vec<Vec<f32>>,
}

/// Computes a 32-band STFT spectrogram (`0..8000 Hz`, 250 Hz/band) and 6-band energy summary
/// from a validated mono 16kHz PCM16 WAV buffer.
pub fn compute_sample_spectrogram_from_wav(wav_bytes: &[u8]) -> Result<SampleSpectrogram> {
    const FFT_SIZE: usize = 512;
    const BASE_HOP: usize = 256;
    const MAX_FRAMES: usize = 240;

    let (pcm, metrics) = validate_and_analyze_wav(wav_bytes, None)?;

    let mut padded = if pcm.len() < FFT_SIZE {
        let mut buf = Vec::with_capacity(FFT_SIZE);
        buf.extend_from_slice(&pcm);
        buf.resize(FFT_SIZE, 0);
        buf
    } else {
        pcm
    };

    let hop = if padded.len() <= FFT_SIZE {
        BASE_HOP
    } else {
        let max_start = padded.len() - FFT_SIZE;
        let step_needed = (max_start / (MAX_FRAMES - 1)).max(BASE_HOP);
        step_needed.div_ceil(BASE_HOP) * BASE_HOP
    };

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let scratch_len = fft.get_inplace_scratch_len();
    let mut fft_buf = vec![Complex32::default(); FFT_SIZE];
    let mut scratch = vec![Complex32::default(); scratch_len];

    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|n| {
            let phase = 2.0 * std::f32::consts::PI * n as f32 / FFT_SIZE as f32;
            0.5 * (1.0 - phase.cos())
        })
        .collect();
    let sum_w_sq: f32 = window.iter().map(|&w| w * w).sum::<f32>().max(1.0);
    let pwr_scale = 1.0 / (FFT_SIZE as f32 * sum_w_sq);

    let mut frames_db = Vec::new();
    let mut band_pwr_acc = [0.0f64; 6];

    let mut start = 0usize;
    while start + FFT_SIZE <= padded.len() {
        for i in 0..FFT_SIZE {
            let x = f32::from(padded[start + i]) / 32768.0;
            fft_buf[i] = Complex32::new(x * window[i], 0.0);
        }
        fft.process_with_scratch(&mut fft_buf, &mut scratch);

        let mut bin_sum = [0.0f32; 32];
        for (k, bin) in fft_buf.iter().enumerate().take(FFT_SIZE / 2 + 1).skip(1) {
            let weight = if k < FFT_SIZE / 2 { 2.0 } else { 1.0 };
            let pwr = weight * bin.norm_sqr() * pwr_scale;

            let disp_idx = ((k - 1) / 8).min(31);
            bin_sum[disp_idx] += pwr;

            let p64 = f64::from(pwr);
            if (2..=5).contains(&k) {
                band_pwr_acc[0] += p64;
            } else if (6..=15).contains(&k) {
                band_pwr_acc[1] += p64;
            } else if (16..=47).contains(&k) {
                band_pwr_acc[2] += p64;
            } else if (48..=127).contains(&k) {
                band_pwr_acc[3] += p64;
            } else if k >= 128 {
                band_pwr_acc[4] += p64;
            }
            if k >= 2 {
                band_pwr_acc[5] += p64;
            }
        }

        let frame_row: Vec<f32> = bin_sum
            .iter()
            .map(|&p| {
                let db = (10.0 * p.max(1e-12).log10()).clamp(-96.0, 0.0);
                (db * 10.0).round() / 10.0
            })
            .collect();
        frames_db.push(frame_row);

        if padded.len() == FFT_SIZE {
            break;
        }
        start += hop;
    }

    let n_frames = frames_db.len().max(1) as f64;
    let to_dbfs = |sum: f64| -> f32 {
        let db = (10.0 * (sum / n_frames).max(1e-12).log10()).clamp(-96.0, 0.0) as f32;
        (db * 10.0).round() / 10.0
    };

    let freq_bands_hz: Vec<u16> = (1..=32).map(|b| (b * 250) as u16).collect();
    let _ = &mut padded;

    Ok(SampleSpectrogram {
        sample_rate: metrics.sample_rate,
        fft_size: FFT_SIZE,
        hop_ms: ((hop as f32 * 1000.0 / metrics.sample_rate as f32) * 10.0).round() / 10.0,
        duration_ms: metrics.duration_ms,
        freq_bands_hz,
        band_energies_dbfs: SampleSpectrogramBands {
            sub_bass: to_dbfs(band_pwr_acc[0]),
            low_mid: to_dbfs(band_pwr_acc[1]),
            mid: to_dbfs(band_pwr_acc[2]),
            presence: to_dbfs(band_pwr_acc[3]),
            brilliance: to_dbfs(band_pwr_acc[4]),
            full: to_dbfs(band_pwr_acc[5]),
        },
        frames_db,
    })
}

/// Encodes raw PCM16 samples into standard mono 16kHz WAV format bytes.
pub fn encode_wav_pcm16_mono(samples: &[i16]) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let spec = WavSpec {
            channels: TARGET_CHANNELS,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: TARGET_BITS_PER_SAMPLE,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::new(&mut cursor, spec).context("Failed to init WavWriter")?;
        for &s in samples {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

/// Validates that an object ID contains only safe alphanumeric characters and underscores,
/// preventing any path traversal or shell manipulation.
pub fn validate_sample_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 64,
        "Sample ID must be between 1 and 64 characters"
    );
    ensure!(
        id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "Sample ID must only contain ASCII alphanumeric characters and underscores (path traversal rejected)"
    );
    Ok(())
}

pub struct SamplesStore {
    base_dir: PathBuf,
    state: Arc<Mutex<SamplesIndex>>,
}

impl SamplesStore {
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&base_dir).context("Failed to create samples store directory")?;
        let index_path = base_dir.join("index.json");
        let index = if index_path.exists() {
            let data = std::fs::read_to_string(&index_path)
                .context("Failed to read samples index.json")?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            SamplesIndex::default()
        };

        Ok(Self {
            base_dir,
            state: Arc::new(Mutex::new(index)),
        })
    }

    fn save_index_locked(&self, index: &SamplesIndex) -> Result<()> {
        let serialized = serde_json::to_string_pretty(index)?;
        let tmp_path = self.base_dir.join("index.json.tmp");
        let final_path = self.base_dir.join("index.json");

        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            use std::io::Write;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    pub fn list(
        &self,
        include_archived: bool,
        split_filter: Option<SampleSplit>,
        label_filter: Option<SampleLabel>,
    ) -> Vec<SampleMetadata> {
        let g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<SampleMetadata> = g
            .samples
            .values()
            .filter(|s| {
                if !include_archived && s.archived {
                    return false;
                }
                if let Some(sp) = split_filter {
                    if s.split != sp {
                        return false;
                    }
                }
                if let Some(lb) = label_filter {
                    if s.label != lb {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        list
    }

    pub fn get_metadata(&self, id: &str) -> Option<SampleMetadata> {
        if validate_sample_id(id).is_err() {
            return None;
        }
        let g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.samples.get(id).cloned()
    }

    pub fn get_wav_bytes(&self, id: &str) -> Result<Vec<u8>> {
        validate_sample_id(id)?;
        let path = self.base_dir.join(format!("{id}.wav"));
        ensure!(path.exists(), "Sample WAV file not found: {id}");
        std::fs::read(&path).with_context(|| format!("Failed to read sample WAV {id}"))
    }

    pub fn get_spectrogram(&self, id: &str) -> Result<SampleSpectrogram> {
        let wav = self.get_wav_bytes(id)?;
        compute_sample_spectrogram_from_wav(&wav)
    }

    pub fn store_sample(&self, dto: CreateSampleDto, wav_bytes: &[u8]) -> Result<SampleMetadata> {
        ensure!(!dto.name.trim().is_empty(), "Sample name must not be empty");
        let (_pcm, metrics) = validate_and_analyze_wav(wav_bytes, dto.interval)?;
        let sha256 = compute_sha256_hex(wav_bytes);

        let id = format!("smp_{}", uuid::Uuid::new_v4().simple());
        validate_sample_id(&id)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let metadata = SampleMetadata {
            id: id.clone(),
            name: dto.name.trim().to_string(),
            source: dto.source.unwrap_or(SampleSource::Upload),
            label: dto.label,
            split: dto.split,
            interval: dto.interval,
            metrics,
            notes: dto.notes.filter(|n| !n.trim().is_empty()),
            archived: false,
            created_at: now,
            sha256,
        };

        // Write WAV file with fsync
        let tmp_wav = self.base_dir.join(format!("{id}.wav.tmp"));
        let final_wav = self.base_dir.join(format!("{id}.wav"));
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_wav)?;
            use std::io::Write;
            file.write_all(wav_bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp_wav, &final_wav)?;

        // Update index atomically
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.samples.insert(id, metadata.clone());
        self.save_index_locked(&g)?;

        Ok(metadata)
    }

    pub fn update_sample(&self, id: &str, dto: UpdateSampleDto) -> Result<SampleMetadata> {
        validate_sample_id(id)?;
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let sample = g
            .samples
            .get_mut(id)
            .with_context(|| format!("Sample not found: {id}"))?;

        if let Some(name) = dto.name {
            ensure!(!name.trim().is_empty(), "Sample name cannot be empty");
            sample.name = name.trim().to_string();
        }
        if let Some(label) = dto.label {
            sample.label = label;
        }
        if let Some(split) = dto.split {
            sample.split = split;
        }
        if let Some(interval) = dto.interval {
            ensure!(
                interval.start_ms < interval.end_ms,
                "Interval start_ms must be before end_ms"
            );
            sample.interval = Some(interval);
        }
        if let Some(notes) = dto.notes {
            sample.notes = if notes.trim().is_empty() {
                None
            } else {
                Some(notes)
            };
        }
        if let Some(archived) = dto.archived {
            sample.archived = archived;
        }

        let updated = sample.clone();
        self.save_index_locked(&g)?;
        Ok(updated)
    }

    pub fn archive_sample(&self, id: &str) -> Result<SampleMetadata> {
        self.update_sample(
            id,
            UpdateSampleDto {
                name: None,
                label: None,
                split: None,
                interval: None,
                notes: None,
                archived: Some(true),
            },
        )
    }

    pub fn delete_sample(&self, id: &str) -> Result<()> {
        validate_sample_id(id)?;
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            g.samples.remove(id).is_some(),
            "Sample {id} not found in index"
        );
        self.save_index_locked(&g)?;

        let wav_path = self.base_dir.join(format!("{id}.wav"));
        if wav_path.exists() {
            let _ = std::fs::remove_file(wav_path);
        }
        Ok(())
    }

    /// Strict dataset split accessor: ensures that `HeldOut` samples are never
    /// mixed into training or calibration sets.
    pub fn get_dataset_split(&self, split: SampleSplit) -> Vec<SampleMetadata> {
        let g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.samples
            .values()
            .filter(|s| !s.archived && s.split == split)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_standard_vectors() {
        assert_eq!(
            compute_sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            compute_sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_validate_wav_valid_pcm16_mono() {
        let pcm = vec![500i16; 16000]; // 1 second mono
        let wav_bytes = encode_wav_pcm16_mono(&pcm).expect("encode");
        let (decoded, metrics) =
            validate_and_analyze_wav(&wav_bytes, None).expect("validate valid");

        assert_eq!(decoded.len(), 16000);
        assert_eq!(metrics.channels, 1);
        assert_eq!(metrics.sample_rate, 16000);
        assert_eq!(metrics.bits_per_sample, 16);
        assert_eq!(metrics.duration_ms, 1000);
        assert_eq!(metrics.peak_amplitude, 500);
        assert!(!metrics.is_clipped);
    }

    #[test]
    fn test_validate_wav_rejects_stereo_and_bad_sample_rates() {
        // Stereo
        let mut cursor = Cursor::new(Vec::new());
        {
            let spec = WavSpec {
                channels: 2,
                sample_rate: 16000,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            };
            let mut writer = WavWriter::new(&mut cursor, spec).unwrap();
            writer.write_sample(100i16).unwrap();
            writer.write_sample(100i16).unwrap();
            writer.finalize().unwrap();
        }
        let err = validate_and_analyze_wav(&cursor.into_inner(), None).unwrap_err();
        assert!(err.to_string().contains("1 channel"));

        // 44.1 kHz
        let mut cursor = Cursor::new(Vec::new());
        {
            let spec = WavSpec {
                channels: 1,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            };
            let mut writer = WavWriter::new(&mut cursor, spec).unwrap();
            writer.write_sample(100i16).unwrap();
            writer.finalize().unwrap();
        }
        let err = validate_and_analyze_wav(&cursor.into_inner(), None).unwrap_err();
        assert!(err.to_string().contains("16000 Hz"));
    }

    #[test]
    fn test_validate_wav_rejects_exceeding_duration_and_truncated() {
        // Exceeding 60 seconds (61 s * 16000 = 976000 samples)
        let long_pcm = vec![10i16; 16000 * 61];
        let wav_bytes = encode_wav_pcm16_mono(&long_pcm).expect("encode");
        let err = validate_and_analyze_wav(&wav_bytes, None).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum limit"));

        // Truncated WAV (header claims data but bytes cut short)
        let valid = encode_wav_pcm16_mono(&vec![100i16; 1600]).unwrap();
        let truncated = &valid[..valid.len() - 100];
        let err = validate_and_analyze_wav(truncated, None).unwrap_err();
        assert!(err.to_string().contains("Truncated or corrupted"));
    }

    #[test]
    fn test_i16_min_clipping_detection_no_panic() {
        let mut pcm = vec![200i16; 1600];
        pcm[100] = i16::MIN; // -32768
        let wav_bytes = encode_wav_pcm16_mono(&pcm).expect("encode");
        let (_decoded, metrics) =
            validate_and_analyze_wav(&wav_bytes, None).expect("validation should succeed");

        assert_eq!(metrics.peak_amplitude, 32768);
        assert!(metrics.is_clipped);
    }

    #[test]
    fn test_samples_store_lifecycle_and_split_isolation() {
        let temp_dir = std::env::temp_dir().join(format!("test_samples_{}", uuid::Uuid::new_v4()));
        let store = SamplesStore::new(temp_dir.clone()).expect("init store");

        let pcm = vec![1200i16; 8000]; // 0.5 sec
        let wav_bytes = encode_wav_pcm16_mono(&pcm).expect("encode");

        // 1. Store train sample
        let train_meta = store
            .store_sample(
                CreateSampleDto {
                    name: "Алиса 1 (обучение)".into(),
                    source: Some(SampleSource::Recorded),
                    label: SampleLabel::PositiveWakeWord,
                    split: SampleSplit::Train,
                    interval: Some(SampleInterval {
                        start_ms: 50,
                        end_ms: 450,
                    }),
                    notes: Some("Обучающий эталон".into()),
                    wav_base64: None,
                },
                &wav_bytes,
            )
            .expect("store train sample");

        // 2. Store held-out sample
        let held_out_meta = store
            .store_sample(
                CreateSampleDto {
                    name: "Алиса 2 (контроль)".into(),
                    source: Some(SampleSource::Upload),
                    label: SampleLabel::PositiveWakeWord,
                    split: SampleSplit::HeldOut,
                    interval: None,
                    notes: Some("Строго контрольная выборка".into()),
                    wav_base64: None,
                },
                &wav_bytes,
            )
            .expect("store held out sample");

        // 3. Verify get_metadata and download WAV
        let loaded_meta = store
            .get_metadata(&train_meta.id)
            .expect("metadata should exist");
        assert_eq!(loaded_meta.name, "Алиса 1 (обучение)");
        assert_eq!(loaded_meta.metrics.duration_ms, 500);

        let downloaded_bytes = store.get_wav_bytes(&train_meta.id).expect("download wav");
        assert_eq!(downloaded_bytes, wav_bytes);

        // 4. Verify split isolation: train split NEVER contains held-out sample
        let train_set = store.get_dataset_split(SampleSplit::Train);
        assert_eq!(train_set.len(), 1);
        assert_eq!(train_set[0].id, train_meta.id);
        assert!(train_set.iter().all(|s| s.split != SampleSplit::HeldOut));

        let held_out_set = store.get_dataset_split(SampleSplit::HeldOut);
        assert_eq!(held_out_set.len(), 1);
        assert_eq!(held_out_set[0].id, held_out_meta.id);

        // 5. Verify path traversal rejection
        assert!(validate_sample_id("../etc/passwd").is_err());
        assert!(validate_sample_id("smp_123/evil").is_err());
        assert!(store.get_wav_bytes("../etc/passwd").is_err());

        // 6. Archive and delete
        let archived = store.archive_sample(&train_meta.id).expect("archive");
        assert!(archived.archived);
        assert_eq!(store.list(false, None, None).len(), 1); // archived excluded by default
        assert_eq!(store.list(true, None, None).len(), 2);

        store.delete_sample(&held_out_meta.id).expect("delete");
        assert!(store.get_metadata(&held_out_meta.id).is_none());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_sample_spectrogram_detects_1khz_peak() {
        let mut samples = Vec::with_capacity(8000);
        for i in 0..8000 {
            let t = i as f32 / 16000.0;
            let s = ((2.0 * std::f32::consts::PI * 1000.0 * t).sin() * 12000.0) as i16;
            samples.push(s);
        }
        let wav = encode_wav_pcm16_mono(&samples).expect("wav");
        let spec = compute_sample_spectrogram_from_wav(&wav).expect("spectrogram");
        assert_eq!(spec.freq_bands_hz.len(), 32);
        assert!(!spec.frames_db.is_empty());
        assert!(
            spec.band_energies_dbfs.mid > spec.band_energies_dbfs.sub_bass + 20.0,
            "1 kHz tone must dominate Mid band: {:?}",
            spec.band_energies_dbfs
        );
        assert!(
            spec.band_energies_dbfs.mid > spec.band_energies_dbfs.brilliance + 20.0,
            "1 kHz tone must exceed Brilliance band: {:?}",
            spec.band_energies_dbfs
        );
        // Band 3 covers 750..1000 Hz, band 4 covers 1000..1250 Hz (1 kHz hits bin 32 -> band 3/4)
        let mid_frame = &spec.frames_db[spec.frames_db.len() / 2];
        assert!(mid_frame[3].max(mid_frame[4]) > -15.0);
    }
}
