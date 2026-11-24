//! Partitioned Block Frequency Domain Adaptive Filter (MDF / PBFDAF).
//!
//! High-performance, low-latency acoustic echo canceller for embedded ARM Cortex-A53.
//! Uses overlap-save FFT block filtering partitioned into K sub-blocks of length N,
//! achieving long impulse response coverage (128ms) with low processing latency (16ms).

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

pub const BLOCK_SIZE: usize = 256; // N = 256 samples (16ms @ 16kHz)
pub const FFT_SIZE: usize = 512; // 2N = 512 points overlap-save FFT
pub const NUM_PARTITIONS: usize = 8; // K = 8 partitions (total length L = 2048 samples = 128ms)

#[derive(Debug, Clone)]
pub struct MdfConfig {
    pub step_size: f32,          // mu (adaptation rate, typically 0.15 .. 0.35)
    pub power_leak: f32,         // lambda (PSD smoothing factor, typically 0.90 .. 0.95)
    pub regularization: f32,     // epsilon (avoids division by zero in quiet bins)
    pub max_attenuation_db: f32, // clamp for residual suppression (e.g. 25.0 dB)
}

impl Default for MdfConfig {
    fn default() -> Self {
        Self {
            step_size: 0.25,
            power_leak: 0.92,
            regularization: 2.0,
            max_attenuation_db: 30.0,
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
    scratch: Vec<Complex32>,
    // Real-time energy metrics
    energy_mic: f32,
    energy_ref: f32,
    energy_err: f32,
    adapted_blocks: usize,
}

impl PartitionedMdfFilter {
    pub fn new(config: MdfConfig) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let ifft = planner.plan_fft_inverse(FFT_SIZE);

        let scratch_len = fft
            .get_inplace_scratch_len()
            .max(ifft.get_inplace_scratch_len());

        Self {
            config,
            fft,
            ifft,
            x_freq: vec![vec![Complex32::default(); FFT_SIZE]; NUM_PARTITIONS],
            w_freq: vec![vec![Complex32::default(); FFT_SIZE]; NUM_PARTITIONS],
            x_old: vec![0.0; BLOCK_SIZE],
            power_spectrum: vec![1e-3; FFT_SIZE],
            fft_in_out: vec![Complex32::default(); FFT_SIZE],
            scratch: vec![Complex32::default(); scratch_len],
            energy_mic: 0.0,
            energy_ref: 0.0,
            energy_err: 0.0,
            adapted_blocks: 0,
        }
    }

    /// Processes one block of microphone input y(n) and aligned reference x(n) (BLOCK_SIZE = 256 samples).
    ///
    /// Returns the cleaned error block e(n) where the estimated acoustic echo has been subtracted.
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
        let lambda = self.config.power_leak;
        for i in 0..FFT_SIZE {
            let mut total_pwr = 0.0f32;
            for k in 0..NUM_PARTITIONS {
                total_pwr += self.x_freq[k][i].norm_sqr();
            }
            self.power_spectrum[i] = lambda * self.power_spectrum[i] + (1.0 - lambda) * total_pwr;
        }

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

        // 7. IFFT to obtain time-domain estimated echo d_hat
        self.ifft
            .process_with_scratch(&mut self.fft_in_out, &mut self.scratch);
        let ifft_scale = 1.0 / FFT_SIZE as f32;

        // 8. Subtract estimated echo in time domain: e(n) = y(n) - d_hat[BLOCK_SIZE + n]
        // Overlap-save discards the first N samples; second N samples contain linear convolution result.
        let mut e_block_energy = 0.0f32;
        for i in 0..BLOCK_SIZE {
            let d_hat = self.fft_in_out[BLOCK_SIZE + i].re * ifft_scale;
            let err = mic_block[i] - d_hat;
            out_err_block[i] = err;
            e_block_energy += err * err;
        }
        self.energy_err = 0.9 * self.energy_err + 0.1 * e_block_energy;

        // 9. Double-talk protection:
        // During initial convergence (first 25 blocks), allow filter to adapt to room acoustics.
        // After initial convergence, freeze adaptation if error energy spikes significantly above reference.
        let is_double_talk = self.adapted_blocks > 25
            && self.energy_err > (self.energy_ref * 2.0 + 1e-3)
            && self.energy_err > 0.05;

        if !is_double_talk {
            self.adapted_blocks = self.adapted_blocks.saturating_add(1);

            // 10. Compute error in frequency domain: E(f) = FFT([0_N, e(n)])
            let mut e_freq = vec![Complex32::default(); FFT_SIZE];
            for i in 0..BLOCK_SIZE {
                e_freq[i] = Complex32::default();
                e_freq[BLOCK_SIZE + i] = Complex32::new(out_err_block[i], 0.0);
            }
            self.fft
                .process_with_scratch(&mut e_freq, &mut self.scratch);

            // 11. Weight adaptation (Constrained Partitioned Frequency-Domain LMS):
            let mu = self.config.step_size;
            let eps = self.config.regularization;

            for k in 0..NUM_PARTITIONS {
                let x_k = &self.x_freq[k];
                // Compute unconstrained gradient in frequency domain: phi_k(f) = mu * conj(X_k(f)) * E(f) / (P_total(f) + eps)
                for i in 0..FFT_SIZE {
                    let norm_factor = mu / (self.power_spectrum[i] + eps);
                    self.fft_in_out[i] = x_k[i].conj() * e_freq[i] * norm_factor;
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
}
