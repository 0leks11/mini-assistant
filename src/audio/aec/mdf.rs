//! Partitioned Block Frequency Domain Adaptive Filter (MDF / PBFDAF).
//!
//! High-performance, low-latency acoustic echo canceller for embedded ARM Cortex-A53.
//! Uses overlap-save FFT block filtering partitioned into K sub-blocks of length N,
//! achieving long impulse response coverage (128ms) with low processing latency (16ms).

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use super::res::{ResConfig, ResidualEchoSuppressor};

pub const BLOCK_SIZE: usize = 256; // N = 256 samples (16ms @ 16kHz)
pub const FFT_SIZE: usize = 512; // 2N = 512 points overlap-save FFT
pub const NUM_PARTITIONS: usize = 8; // K = 8 partitions (total length L = 2048 samples = 128ms)

#[derive(Debug, Clone)]
pub struct MdfConfig {
    pub step_size: f32,          // mu (adaptation rate, typically 0.15 .. 0.35)
    pub power_leak: f32,         // lambda (PSD smoothing factor, typically 0.90 .. 0.95)
    pub regularization: f32,     // epsilon (avoids division by zero in quiet bins)
    pub max_attenuation_db: f32, // clamp for residual suppression (e.g. 25.0 dB)
    pub res: ResConfig,
}

impl Default for MdfConfig {
    fn default() -> Self {
        Self {
            step_size: 0.75,
            power_leak: 0.92,
            regularization: 0.02,
            max_attenuation_db: 30.0,
            res: ResConfig::default(),
        }
    }
}

pub struct PartitionedMdfFilter {
    config: MdfConfig,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    // History of reference signals in frequency domain: K partitions of size 2N
    x_freq: Vec<Vec<Complex32>>,
    // Adaptive filter weights in frequency domain: K partitions of size 2N
    w_freq: Vec<Vec<Complex32>>,
    // Reference signal time-domain history (last N samples for overlap-save)
    x_old: Vec<f32>,
    // Smoothed power spectral density per frequency bin
    power_spectrum: Vec<f32>,
    // Working buffers for FFT/IFFT (preallocated to guarantee zero runtime allocations)
    fft_in_out: Vec<Complex32>,
    d_hat_freq: Vec<Complex32>,
    e_freq: Vec<Complex32>,
    scratch: Vec<Complex32>,
    res: ResidualEchoSuppressor,
    // Real-time energy metrics
    energy_mic: f32,
    energy_ref: f32,
    energy_err: f32,
    coupling_gain_sq: f32,
    adapted_blocks: usize,
    dt_streak: usize,
    last_double_talk: bool,
}

impl PartitionedMdfFilter {
    pub fn new(config: MdfConfig) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let ifft = planner.plan_fft_inverse(FFT_SIZE);

        let scratch_len = fft
            .get_inplace_scratch_len()
            .max(ifft.get_inplace_scratch_len());

        let res = ResidualEchoSuppressor::new(config.res.clone());

        Self {
            config,
            fft,
            ifft,
            x_freq: vec![vec![Complex32::default(); FFT_SIZE]; NUM_PARTITIONS],
            w_freq: vec![vec![Complex32::default(); FFT_SIZE]; NUM_PARTITIONS],
            x_old: vec![0.0; BLOCK_SIZE],
            power_spectrum: vec![1e-3; FFT_SIZE],
            fft_in_out: vec![Complex32::default(); FFT_SIZE],
            d_hat_freq: vec![Complex32::default(); FFT_SIZE],
            e_freq: vec![Complex32::default(); FFT_SIZE],
            scratch: vec![Complex32::default(); scratch_len],
            res,
            energy_mic: 0.0,
            energy_ref: 0.0,
            energy_err: 0.0,
            coupling_gain_sq: 1.0,
            adapted_blocks: 0,
            dt_streak: 0,
            last_double_talk: false,
        }
    }

    /// Processes one block of microphone input y(n) and aligned reference x(n) (BLOCK_SIZE = 256 samples).
    ///
    /// Returns the cleaned error block e(n) where the estimated acoustic echo and residual harmonics have been suppressed.
    pub fn process_block(
        &mut self,
        mic_block: &[f32],
        ref_block: &[f32],
        out_err_block: &mut [f32],
    ) {
        assert_eq!(mic_block.len(), BLOCK_SIZE);
        assert_eq!(ref_block.len(), BLOCK_SIZE);
        assert_eq!(out_err_block.len(), BLOCK_SIZE);

        // 1. Compute energies for telemetry and DTD
        let e_mic: f32 = mic_block.iter().map(|&s| s * s).sum();
        let e_ref: f32 = ref_block.iter().map(|&s| s * s).sum();
        self.energy_mic = 0.9 * self.energy_mic + 0.1 * e_mic;
        self.energy_ref = 0.9 * self.energy_ref + 0.1 * e_ref;

        // 2. Prepare reference 2N overlap block: [x_old (N), ref_block (N)]
        for (i, &sample) in ref_block.iter().enumerate().take(BLOCK_SIZE) {
            self.fft_in_out[i] = Complex32::new(self.x_old[i], 0.0);
            self.fft_in_out[BLOCK_SIZE + i] = Complex32::new(sample, 0.0);
            self.x_old[i] = sample;
        }

        // 3. FFT of reference block: X_0(m)
        self.fft
            .process_with_scratch(&mut self.fft_in_out, &mut self.scratch);

        // 4. Shift partition history in frequency domain: X_k = X_{k-1}
        for k in (1..NUM_PARTITIONS).rev() {
            let (left, right) = self.x_freq.split_at_mut(k);
            right[0].copy_from_slice(&left[k - 1]);
        }
        self.x_freq[0].copy_from_slice(&self.fft_in_out);

        // 5. Update power spectral density across all active partitions: P_total(f)
        // Fast attack (.max(total_pwr)) prevents cold-start/transient overshoot when PSD lags.
        let lambda = self.config.power_leak;
        let mut sum_total_pwr = 0.0f32;
        for i in 0..FFT_SIZE {
            let mut total_pwr = 0.0f32;
            for k in 0..NUM_PARTITIONS {
                total_pwr += self.x_freq[k][i].norm_sqr();
            }
            let ema = lambda * self.power_spectrum[i] + (1.0 - lambda) * total_pwr;
            self.power_spectrum[i] = ema.max(total_pwr);
            sum_total_pwr += self.power_spectrum[i];
        }
        let dynamic_reg = self
            .config
            .regularization
            .max(0.05 * (sum_total_pwr / FFT_SIZE as f32));

        // 6. Frequency-domain convolution: D_hat(f) = Sum_k W_k(f) * X_k(f)
        for i in 0..FFT_SIZE {
            self.fft_in_out[i] = Complex32::default();
        }
        for k in 0..NUM_PARTITIONS {
            let w_k = &self.w_freq[k];
            let x_k = &self.x_freq[k];
            for i in 0..FFT_SIZE {
                self.fft_in_out[i] += w_k[i] * x_k[i];
            }
        }
        // Save estimated echo spectrum before IFFT overwrites fft_in_out
        self.d_hat_freq.copy_from_slice(&self.fft_in_out);

        // 7. IFFT to obtain time-domain estimated echo d_hat
        self.ifft
            .process_with_scratch(&mut self.fft_in_out, &mut self.scratch);
        let ifft_scale = 1.0 / FFT_SIZE as f32;

        // 8. Subtract estimated echo in time domain: e(n) = y(n) - d_hat[BLOCK_SIZE + n]
        // Overlap-save discards the first N samples; second N samples contain linear convolution result.
        let mut e_linear_energy = 0.0f32;
        for i in 0..BLOCK_SIZE {
            let d_hat = self.fft_in_out[BLOCK_SIZE + i].re * ifft_scale;
            let err = mic_block[i] - d_hat;
            out_err_block[i] = err;
            e_linear_energy += err * err;
        }

        // 9. Double-talk protection:
        // Compare residual error energy against BOTH coupling-scaled acoustic echo estimate
        // (handles real Mic/Ref ~ 0.10 without freezing initial convergence) and Geigel ratio.
        let is_warmup = self.adapted_blocks <= 25;
        let expected_echo_inst = e_ref * self.coupling_gain_sq;
        let expected_echo_avg = self.energy_ref * self.coupling_gain_sq;
        let dt_acoustic = (e_linear_energy > (expected_echo_inst * 2.0 + 0.003)
            && e_mic > (expected_echo_inst * 1.8 + 0.003))
            || (self.energy_err > (expected_echo_avg * 2.0 + 0.004)
                && self.energy_mic > (expected_echo_avg * 1.8 + 0.004));
        let dt_geigel = (e_linear_energy > (e_ref * 1.5 + 1e-3)
            || self.energy_err > (self.energy_ref * 1.5 + 1e-3))
            && (e_linear_energy > 0.005 || self.energy_err > 0.01);
        let raw_dt = !is_warmup && (dt_acoustic || dt_geigel);
        let is_double_talk = if raw_dt {
            self.dt_streak = self.dt_streak.saturating_add(1);
            if self.dt_streak > 90 {
                // Guard against deadlock if speaker position/volume jumps significantly
                self.coupling_gain_sq = 1.0;
                self.dt_streak = 0;
                false
            } else {
                true
            }
        } else {
            self.dt_streak = 0;
            false
        };
        self.last_double_talk = is_double_talk;

        if !is_double_talk && e_ref > 1e-3 && e_mic > 1e-5 && e_linear_energy < e_mic * 0.35 {
            let inst_gain_sq = (e_mic / e_ref).clamp(0.002, 2.0);
            self.coupling_gain_sq = 0.9 * self.coupling_gain_sq + 0.1 * inst_gain_sq;
        }

        // 10. Compute linear error in frequency domain: E(f) = FFT([0_N, e(n)])
        for (i, &err_val) in out_err_block.iter().enumerate().take(BLOCK_SIZE) {
            self.e_freq[i] = Complex32::default();
            self.e_freq[BLOCK_SIZE + i] = Complex32::new(err_val, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.e_freq, &mut self.scratch);

        // 11. Apply Non-linear Residual Echo Suppression (RES Wiener Post-Filter)
        self.res.apply(
            &self.e_freq,
            &self.d_hat_freq,
            is_double_talk,
            is_warmup,
            out_err_block,
        );

        // 12. Update final error energy for ERLE and telemetry metrics
        let e_block_energy: f32 = out_err_block.iter().map(|&s| s * s).sum();
        self.energy_err = 0.9 * self.energy_err + 0.1 * e_block_energy;

        // 13. Weight adaptation (Constrained Partitioned Frequency-Domain LMS):
        if !is_double_talk {
            self.adapted_blocks = self.adapted_blocks.saturating_add(1);

            let mu = self.config.step_size;
            let eps = dynamic_reg;

            for k in 0..NUM_PARTITIONS {
                let x_k = &self.x_freq[k];
                // Compute unconstrained gradient in frequency domain: phi_k(f) = mu * conj(X_k(f)) * E(f) / (P_total(f) + eps)
                for (i, x_val) in x_k.iter().enumerate().take(FFT_SIZE) {
                    let norm_factor = mu / (self.power_spectrum[i] + eps);
                    self.fft_in_out[i] = x_val.conj() * self.e_freq[i] * norm_factor;
                }

                // Transform gradient to time domain
                self.ifft
                    .process_with_scratch(&mut self.fft_in_out, &mut self.scratch);

                // Apply time-domain constraint: zero-out the last N samples to eliminate circular aliasing
                for i in 0..BLOCK_SIZE {
                    self.fft_in_out[i] = Complex32::new(self.fft_in_out[i].re * ifft_scale, 0.0);
                    self.fft_in_out[BLOCK_SIZE + i] = Complex32::default();
                }

                // Transform constrained gradient back to frequency domain
                self.fft
                    .process_with_scratch(&mut self.fft_in_out, &mut self.scratch);

                // Update partition weights
                let w_k = &mut self.w_freq[k];
                for (w_bin, grad_bin) in w_k.iter_mut().zip(&self.fft_in_out) {
                    *w_bin += *grad_bin;
                }
            }
        }
    }

    /// Echo Return Loss Enhancement (ERLE in dB):
    /// ERLE = 10 * log10(E_mic / E_error).
    /// Typically 20 .. 35 dB during single-talk echo cancellation.
    pub fn current_erle_db(&self) -> f32 {
        if self.energy_err < 1e-9 || self.energy_mic < 1e-9 {
            0.0
        } else {
            10.0 * (self.energy_mic / self.energy_err).log10()
        }
    }

    pub fn energy_metrics(&self) -> (f32, f32, f32) {
        (self.energy_mic, self.energy_ref, self.energy_err)
    }

    pub fn is_double_talk(&self) -> bool {
        self.last_double_talk
    }
}
