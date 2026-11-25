//! Acoustic Echo Cancellation (AEC) Subsystem for GPT Mini.
//!
//! Integrates:
//! - DelayBuffer: Hardware & acoustic propagation alignment (tau_bulk = 16 samples @ 16kHz);
//! - PartitionedMdfFilter: 8-partition frequency domain adaptive filter (128ms tail);
//! - Double-Talk Detector: Geigel energy ratio to protect near-end voice;
//! - Zero-regression fallback: transparent passthrough when AEC is disabled in config.toml.

#![allow(dead_code)]

pub mod mdf;
pub mod res;
pub mod ring_buffer;

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use self::mdf::{MdfConfig, PartitionedMdfFilter, BLOCK_SIZE};
use self::res::ResConfig;
use self::ring_buffer::DelayBuffer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AecConfig {
    pub enabled: bool,
    pub bulk_delay_samples: usize,
    pub step_size: f32,
    pub power_leak: f32,
    pub regularization: f32,
    pub max_attenuation_db: f32,
    #[serde(default = "default_true")]
    pub res_enabled: bool,
    #[serde(default = "default_res_floor_db")]
    pub res_floor_db: f32,
    #[serde(default = "default_res_overdrive")]
    pub res_overdrive: f32,
}

fn default_true() -> bool {
    true
}

fn default_res_floor_db() -> f32 {
    -24.0
}

fn default_res_overdrive() -> f32 {
    1.5
}

impl Default for AecConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bulk_delay_samples: 16, // 1.00ms causal alignment (direct acoustic peak ~29 samples)
            step_size: 0.75,
            power_leak: 0.92,
            regularization: 0.02,
            max_attenuation_db: 30.0,
            res_enabled: true,
            res_floor_db: -24.0,
            res_overdrive: 1.5,
        }
    }
}

impl AecConfig {
    pub fn default_mini() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AecMetrics {
    pub erle_db: f32,
    pub mic_rms_dbfs: f32,
    pub ref_rms_dbfs: f32,
    pub err_rms_dbfs: f32,
    pub double_talk_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AecTelemetrySnapshot {
    pub enabled: bool,
    pub dtd_status: String,
    pub erle_db: f32,
    pub mic_rms_dbfs: f32,
    pub ref_rms_dbfs: f32,
    pub err_rms_dbfs: f32,
    pub double_talk_active: bool,
    pub blocks_processed: u64,
    pub adapted_blocks: u64,
    pub config: AecConfig,
}

/// Lock-free runtime telemetry and enable toggle shared between the ALSA capture thread and HTTP API.
#[derive(Debug)]
pub struct SharedAecTelemetry {
    enabled: AtomicBool,
    erle_db_bits: AtomicU32,
    mic_rms_dbfs_bits: AtomicU32,
    ref_rms_dbfs_bits: AtomicU32,
    err_rms_dbfs_bits: AtomicU32,
    double_talk_active: AtomicBool,
    blocks_processed: AtomicU64,
    adapted_blocks: AtomicU64,
    config: Mutex<AecConfig>,
}

impl SharedAecTelemetry {
    /// Creates a new shared telemetry store initialized from `config`.
    pub fn new(config: AecConfig) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(config.enabled),
            erle_db_bits: AtomicU32::new(0.0_f32.to_bits()),
            mic_rms_dbfs_bits: AtomicU32::new((-120.0_f32).to_bits()),
            ref_rms_dbfs_bits: AtomicU32::new((-120.0_f32).to_bits()),
            err_rms_dbfs_bits: AtomicU32::new((-120.0_f32).to_bits()),
            double_talk_active: AtomicBool::new(false),
            blocks_processed: AtomicU64::new(0),
            adapted_blocks: AtomicU64::new(0),
            config: Mutex::new(config),
        })
    }

    /// Returns whether AEC processing is currently enabled.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Dynamically enables or disables AEC processing without restarting audio capture.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
        cfg.enabled = enabled;
    }

    /// Publishes block telemetry from the real-time capture thread without locks.
    pub fn publish(&self, metrics: &AecMetrics, adapted_blocks: usize) {
        self.erle_db_bits
            .store(metrics.erle_db.to_bits(), Ordering::Relaxed);
        self.mic_rms_dbfs_bits
            .store(metrics.mic_rms_dbfs.to_bits(), Ordering::Relaxed);
        self.ref_rms_dbfs_bits
            .store(metrics.ref_rms_dbfs.to_bits(), Ordering::Relaxed);
        self.err_rms_dbfs_bits
            .store(metrics.err_rms_dbfs.to_bits(), Ordering::Relaxed);
        self.double_talk_active
            .store(metrics.double_talk_active, Ordering::Relaxed);
        self.adapted_blocks
            .store(adapted_blocks as u64, Ordering::Relaxed);
        self.blocks_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns an instantaneous consistent telemetry snapshot for UI and REST consumers.
    pub fn snapshot(&self) -> AecTelemetrySnapshot {
        let enabled = self.enabled.load(Ordering::SeqCst);
        let erle_db = f32::from_bits(self.erle_db_bits.load(Ordering::Relaxed));
        let mic_rms_dbfs = f32::from_bits(self.mic_rms_dbfs_bits.load(Ordering::Relaxed));
        let ref_rms_dbfs = f32::from_bits(self.ref_rms_dbfs_bits.load(Ordering::Relaxed));
        let err_rms_dbfs = f32::from_bits(self.err_rms_dbfs_bits.load(Ordering::Relaxed));
        let double_talk_active = self.double_talk_active.load(Ordering::Relaxed);
        let blocks_processed = self.blocks_processed.load(Ordering::Relaxed);
        let adapted_blocks = self.adapted_blocks.load(Ordering::Relaxed);
        let config = self
            .config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let dtd_status = if !enabled {
            "DISABLED"
        } else if blocks_processed == 0 {
            "IDLE"
        } else if double_talk_active {
            "DOUBLE_TALK"
        } else if ref_rms_dbfs <= -70.0 {
            "MUTED"
        } else if adapted_blocks < 25 {
            "CONVERGING"
        } else {
            "ACTIVE"
        }
        .to_string();

        AecTelemetrySnapshot {
            enabled,
            dtd_status,
            erle_db,
            mic_rms_dbfs,
            ref_rms_dbfs,
            err_rms_dbfs,
            double_talk_active,
            blocks_processed,
            adapted_blocks,
            config,
        }
    }
}

fn rms_dbfs_pcm16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    const NORM: f32 = 1.0 / 32768.0;
    let mean_sq = samples
        .iter()
        .map(|&s| {
            let v = s as f32 * NORM;
            v * v
        })
        .sum::<f32>()
        / samples.len() as f32;
    let rms = mean_sq.sqrt();
    if rms > 1e-6 {
        20.0 * rms.log10()
    } else {
        -120.0
    }
}

pub struct AecProcessor {
    config: AecConfig,
    delay_buffer: DelayBuffer,
    filter: PartitionedMdfFilter,
    shared_telemetry: Option<Arc<SharedAecTelemetry>>,
    // Intermediate normalized float buffers
    buf_mic_f32: Vec<f32>,
    buf_ref_f32: Vec<f32>,
    buf_aligned_ref: Vec<f32>,
    buf_err_f32: Vec<f32>,
}

impl AecProcessor {
    pub fn new(config: AecConfig) -> Self {
        let delay_buffer = DelayBuffer::new(config.bulk_delay_samples, 1024);
        let mdf_config = MdfConfig {
            step_size: config.step_size,
            power_leak: config.power_leak,
            regularization: config.regularization,
            max_attenuation_db: config.max_attenuation_db,
            res: ResConfig {
                enabled: config.res_enabled,
                floor_db: config.res_floor_db,
                overdrive: config.res_overdrive,
                ..Default::default()
            },
        };
        let filter = PartitionedMdfFilter::new(mdf_config);

        Self {
            config,
            delay_buffer,
            filter,
            shared_telemetry: None,
            buf_mic_f32: vec![0.0; BLOCK_SIZE],
            buf_ref_f32: vec![0.0; BLOCK_SIZE],
            buf_aligned_ref: vec![0.0; BLOCK_SIZE],
            buf_err_f32: vec![0.0; BLOCK_SIZE],
        }
    }

    /// Attach a lock-free shared telemetry & runtime control handle to this processor.
    pub fn with_shared_telemetry(mut self, telemetry: Arc<SharedAecTelemetry>) -> Self {
        self.config.enabled = telemetry.is_enabled();
        self.shared_telemetry = Some(telemetry);
        self
    }

    pub fn has_shared_telemetry(&self) -> bool {
        self.shared_telemetry.is_some()
    }

    /// Default calibrated engine for GPT Mini
    pub fn default_mini() -> Self {
        Self::new(AecConfig::default())
    }

    pub fn is_enabled(&self) -> bool {
        self.shared_telemetry
            .as_ref()
            .map_or(self.config.enabled, |t| t.is_enabled())
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
        if let Some(ref telem) = self.shared_telemetry {
            telem.set_enabled(enabled);
        }
    }

    pub fn set_delay(&mut self, delay_samples: usize) {
        self.config.bulk_delay_samples = delay_samples;
        self.delay_buffer.set_delay(delay_samples);
    }

    /// Processes a block of 16-bit PCM audio (BLOCK_SIZE = 256 samples = 16ms @ 16kHz).
    ///
    /// - `mic_pcm`: 256 samples from 4-mic beamformer y(n).
    /// - `ref_pcm`: 256 samples from DAC loopback reference x(n).
    /// - `out_pcm`: 256 samples buffer where echo-cancelled audio e(n) is written.
    pub fn process_frame(&mut self, mic_pcm: &[i16], ref_pcm: &[i16], out_pcm: &mut [i16]) {
        assert_eq!(
            mic_pcm.len(),
            BLOCK_SIZE,
            "Frame must be exactly 256 samples"
        );
        assert_eq!(
            ref_pcm.len(),
            BLOCK_SIZE,
            "Frame must be exactly 256 samples"
        );
        assert_eq!(
            out_pcm.len(),
            BLOCK_SIZE,
            "Output buffer must be exactly 256 samples"
        );

        if !self.is_enabled() {
            out_pcm.copy_from_slice(mic_pcm);
            if let Some(ref telem) = self.shared_telemetry {
                let mic_db = rms_dbfs_pcm16(mic_pcm);
                let ref_db = rms_dbfs_pcm16(ref_pcm);
                telem.publish(
                    &AecMetrics {
                        erle_db: 0.0,
                        mic_rms_dbfs: mic_db,
                        ref_rms_dbfs: ref_db,
                        err_rms_dbfs: mic_db,
                        double_talk_active: false,
                    },
                    self.filter.adapted_blocks(),
                );
            }
            return;
        }

        const NORM: f32 = 1.0 / 32768.0;

        // 1. Convert to normalized float [-1.0, 1.0]
        for i in 0..BLOCK_SIZE {
            self.buf_mic_f32[i] = mic_pcm[i] as f32 * NORM;
            self.buf_ref_f32[i] = ref_pcm[i] as f32 * NORM;
        }

        // 2. Align reference audio using ring buffer (tau_bulk compensation)
        self.delay_buffer
            .process_block(&self.buf_ref_f32, &mut self.buf_aligned_ref);

        // 3. Process adaptive filter
        self.filter.process_block(
            &self.buf_mic_f32,
            &self.buf_aligned_ref,
            &mut self.buf_err_f32,
        );

        // 4. Convert error signal back to PCM16
        for (out_sample, &err_sample) in out_pcm.iter_mut().zip(&self.buf_err_f32) {
            let clamped = (err_sample * 32768.0).round().clamp(-32768.0, 32767.0);
            *out_sample = clamped as i16;
        }

        if let Some(ref telem) = self.shared_telemetry {
            telem.publish(&self.get_metrics(), self.filter.adapted_blocks());
        }
    }

    pub fn get_metrics(&self) -> AecMetrics {
        let (e_mic, e_ref, e_err) = self.filter.energy_metrics();
        let erle_db = self.filter.current_erle_db();

        let mic_rms = (e_mic / BLOCK_SIZE as f32).sqrt();
        let ref_rms = (e_ref / BLOCK_SIZE as f32).sqrt();
        let err_rms = (e_err / BLOCK_SIZE as f32).sqrt();

        let mic_rms_dbfs = if mic_rms > 1e-6 {
            20.0 * mic_rms.log10()
        } else {
            -120.0
        };
        let ref_rms_dbfs = if ref_rms > 1e-6 {
            20.0 * ref_rms.log10()
        } else {
            -120.0
        };
        let err_rms_dbfs = if err_rms > 1e-6 {
            20.0 * err_rms.log10()
        } else {
            -120.0
        };

        let double_talk_active = self.filter.is_double_talk();

        AecMetrics {
            erle_db,
            mic_rms_dbfs,
            ref_rms_dbfs,
            err_rms_dbfs,
            double_talk_active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_aec_telemetry_status_and_toggle() {
        let cfg = AecConfig {
            enabled: true,
            ..Default::default()
        };
        let telem = SharedAecTelemetry::new(cfg.clone());
        let initial = telem.snapshot();
        assert!(initial.enabled);
        assert_eq!(initial.dtd_status, "IDLE");
        assert_eq!(initial.blocks_processed, 0);

        telem.publish(
            &AecMetrics {
                erle_db: 12.5,
                mic_rms_dbfs: -30.0,
                ref_rms_dbfs: -85.0,
                err_rms_dbfs: -42.5,
                double_talk_active: false,
            },
            5,
        );
        assert_eq!(telem.snapshot().dtd_status, "MUTED");

        telem.publish(
            &AecMetrics {
                erle_db: 18.0,
                mic_rms_dbfs: -20.0,
                ref_rms_dbfs: -25.0,
                err_rms_dbfs: -38.0,
                double_talk_active: false,
            },
            10,
        );
        assert_eq!(telem.snapshot().dtd_status, "CONVERGING");

        telem.publish(
            &AecMetrics {
                erle_db: 29.5,
                mic_rms_dbfs: -20.0,
                ref_rms_dbfs: -25.0,
                err_rms_dbfs: -49.5,
                double_talk_active: false,
            },
            30,
        );
        let active = telem.snapshot();
        assert_eq!(active.dtd_status, "ACTIVE");
        assert!((active.erle_db - 29.5).abs() < 1e-4);

        telem.publish(
            &AecMetrics {
                erle_db: 10.0,
                mic_rms_dbfs: -15.0,
                ref_rms_dbfs: -25.0,
                err_rms_dbfs: -25.0,
                double_talk_active: true,
            },
            30,
        );
        assert_eq!(telem.snapshot().dtd_status, "DOUBLE_TALK");

        // Toggle off cleanly via runtime control
        let mut proc = AecProcessor::new(cfg).with_shared_telemetry(telem.clone());
        assert!(proc.is_enabled());
        telem.set_enabled(false);
        assert!(!proc.is_enabled());

        let mic = [100i16; BLOCK_SIZE];
        let ref_pcm = [50i16; BLOCK_SIZE];
        let mut out = [0i16; BLOCK_SIZE];
        proc.process_frame(&mic, &ref_pcm, &mut out);
        assert_eq!(out, mic);
        let disabled = telem.snapshot();
        assert!(!disabled.enabled);
        assert_eq!(disabled.dtd_status, "DISABLED");
        assert_eq!(disabled.erle_db, 0.0);
    }
}
