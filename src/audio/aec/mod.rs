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
    -18.0
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
            res_floor_db: -18.0,
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

pub struct AecProcessor {
    config: AecConfig,
    delay_buffer: DelayBuffer,
    filter: PartitionedMdfFilter,
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
            buf_mic_f32: vec![0.0; BLOCK_SIZE],
            buf_ref_f32: vec![0.0; BLOCK_SIZE],
            buf_aligned_ref: vec![0.0; BLOCK_SIZE],
            buf_err_f32: vec![0.0; BLOCK_SIZE],
        }
    }

    /// Default calibrated engine for GPT Mini
    pub fn default_mini() -> Self {
        Self::new(AecConfig::default())
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
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

        if !self.config.enabled {
            out_pcm.copy_from_slice(mic_pcm);
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
