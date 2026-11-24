//! Residual Echo Suppression (RES) Wiener Post-Filter for GPT Mini.
//!
//! Suppresses non-linear acoustic echo (harmonic distortion of KRK Systems monitor / amplifier,
//! cabinet resonances, and room reverberation tail exceeding the MDF filter length).
//!
//! Implemented as an adaptive Wiener gain in the frequency domain with:
//! - 3-tap frequency bin smoothing to eliminate musical noise;
//! - Asymmetric time smoothing (fast attack, smooth release);
//! - Double-talk relaxation to preserve user voice ("Штучка");
//! - Guaranteed zero allocations in the audio processing loop.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::mdf::{BLOCK_SIZE, FFT_SIZE};

const HALF_FFT: usize = BLOCK_SIZE; // 256 positive frequency bins (0..256)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResConfig {
    pub enabled: bool,
    /// Minimum suppression floor in dB (e.g. -18.0 dB = 0.126 gain factor)
    pub floor_db: f32,
    /// Overdrive subtraction multiplier (typically 1.2 .. 2.0)
    pub overdrive: f32,
    /// Residual echo leakage factor from estimated echo spectrum (0.05 .. 0.20)
    pub leakage_factor: f32,
    /// Temporal smoothing factor for gain attack (0.3 .. 0.7)
    pub attack_smoothing: f32,
    /// Temporal smoothing factor for gain release (0.7 .. 0.95)
    pub release_smoothing: f32,
}

impl Default for ResConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            floor_db: -18.0,
            overdrive: 1.5,
            leakage_factor: 0.10,
            attack_smoothing: 0.50,
            release_smoothing: 0.85,
        }
    }
}

pub struct ResidualEchoSuppressor {
    config: ResConfig,
    ifft: Arc<dyn Fft<f32>>,
    prev_gain: Vec<f32>,
    raw_gain: Vec<f32>,
    smoothed_freq_gain: Vec<f32>,
    fft_buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    min_gain: f32,
}

impl ResidualEchoSuppressor {
    pub fn new(config: ResConfig) -> Self {
        let mut planner = FftPlanner::new();
        let ifft = planner.plan_fft_inverse(FFT_SIZE);
        let scratch_len = ifft.get_inplace_scratch_len();

        let min_gain = 10.0f32.powf(config.floor_db / 20.0).clamp(0.01, 1.0);

        Self {
            config,
            ifft,
            prev_gain: vec![1.0; HALF_FFT + 1],
            raw_gain: vec![1.0; HALF_FFT + 1],
            smoothed_freq_gain: vec![1.0; HALF_FFT + 1],
            fft_buf: vec![Complex32::default(); FFT_SIZE],
            scratch: vec![Complex32::default(); scratch_len],
            min_gain,
        }
    }

    /// Reset internal state
    pub fn reset(&mut self) {
        self.prev_gain.fill(1.0);
        self.raw_gain.fill(1.0);
        self.smoothed_freq_gain.fill(1.0);
    }

    /// Applies Residual Echo Suppression in frequency domain.
    ///
    /// - `e_freq`: FFT of time-domain linear error `[0_N, e(n)]`.
    /// - `d_hat_freq`: FFT of estimated acoustic echo `D_hat(f)`.
    /// - `is_double_talk`: whether near-end speech is active.
    /// - `is_warmup`: true during initial filter adaptation blocks.
    /// - `out_err_block`: target time-domain slice (BLOCK_SIZE = 256) where suppressed output is written.
    pub fn apply(
        &mut self,
        e_freq: &[Complex32],
        d_hat_freq: &[Complex32],
        is_double_talk: bool,
        is_warmup: bool,
        out_err_block: &mut [f32],
    ) {
        assert_eq!(e_freq.len(), FFT_SIZE);
        assert_eq!(d_hat_freq.len(), FFT_SIZE);
        assert_eq!(out_err_block.len(), BLOCK_SIZE);

        if !self.config.enabled || is_warmup {
            // Passthrough linear error when RES is disabled or during filter warmup
            return;
        }

        if is_double_talk {
            // Relax suppression towards 1.0 (0 dB attenuation) to preserve near-end speech
            self.prev_gain.fill(1.0);
            return;
        }

        // 1. Calculate raw Wiener gain per frequency bin in positive half-spectrum (0..HALF_FFT)
        let eps = 1e-6f32;
        let leakage = self.config.leakage_factor;
        let overdrive = self.config.overdrive;
        let min_g = self.min_gain;

        for i in 0..=HALF_FFT {
            let p_dhat = d_hat_freq[i].norm_sqr();
            let p_err = e_freq[i].norm_sqr();

            if p_dhat < eps {
                self.raw_gain[i] = 1.0;
            } else {
                let p_res_echo = leakage * p_dhat;
                let g = 1.0 - overdrive * (p_res_echo / (p_err + eps));
                self.raw_gain[i] = g.clamp(min_g, 1.0);
            }
        }

        // 2. Frequency-domain smoothing (3-point triangular kernel) to eliminate musical noise
        self.smoothed_freq_gain[0] = self.raw_gain[0];
        self.smoothed_freq_gain[HALF_FFT] = self.raw_gain[HALF_FFT];
        for i in 1..HALF_FFT {
            self.smoothed_freq_gain[i] =
                0.25 * self.raw_gain[i - 1] + 0.50 * self.raw_gain[i] + 0.25 * self.raw_gain[i + 1];
        }

        // 3. Temporal smoothing with asymmetric attack/release
        for i in 0..=HALF_FFT {
            let target = self.smoothed_freq_gain[i];
            let alpha = if target < self.prev_gain[i] {
                self.config.attack_smoothing
            } else {
                self.config.release_smoothing
            };
            self.prev_gain[i] = alpha * self.prev_gain[i] + (1.0 - alpha) * target;
        }

        // 4. Apply symmetric gain to error spectrum E(f)
        for (i, &e_val) in e_freq.iter().enumerate().take(HALF_FFT + 1) {
            let g = self.prev_gain[i];
            self.fft_buf[i] = e_val * g;
        }
        for (i, &e_val) in e_freq.iter().enumerate().take(FFT_SIZE).skip(HALF_FFT + 1) {
            let g = self.prev_gain[FFT_SIZE - i];
            self.fft_buf[i] = e_val * g;
        }

        // 5. IFFT back to time domain
        self.ifft
            .process_with_scratch(&mut self.fft_buf, &mut self.scratch);
        let ifft_scale = 1.0 / FFT_SIZE as f32;

        // 6. Overlap-save: second half [BLOCK_SIZE..2*BLOCK_SIZE] contains the cleaned audio
        for (out_sample, fft_val) in out_err_block
            .iter_mut()
            .zip(&self.fft_buf[BLOCK_SIZE..2 * BLOCK_SIZE])
        {
            *out_sample = fft_val.re * ifft_scale;
        }
    }
}
