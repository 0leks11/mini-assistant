use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::{num_complex::Complex32, Fft, FftPlanner};

pub struct MfccExtractor {
    num_coefficients: usize,
    pre_emphasis_coefficient: f32,
    samples_per_frame: usize,
    samples_per_shift: usize,
    magnitude_spectrum_size: usize,
    sample_rate: usize,
    // state
    filter_bank: Vec<Vec<f32>>,
    hamming_window: Vec<f32>,
    samples: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    fft_buffer: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
    magnitude_spectrum: Vec<f32>,
    mel_spectrum: Vec<f32>,
    dct_cos_table: Vec<f32>,
}

impl MfccExtractor {
    pub fn new(
        sample_rate: usize,
        samples_per_frame: usize,
        samples_per_shift: usize,
        num_coefficients: u16,
        pre_emphasis_coefficient: f32,
    ) -> MfccExtractor {
        let min_frequency = 0;
        let max_frequency = sample_rate / 2;
        let magnitude_spectrum_size = samples_per_frame / 2;
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(samples_per_frame);
        let scratch_len = fft.get_inplace_scratch_len();
        let n_coeff = num_coefficients as usize;
        MfccExtractor {
            samples: Vec::with_capacity(samples_per_frame + samples_per_shift),
            samples_per_shift,
            samples_per_frame,
            pre_emphasis_coefficient,
            num_coefficients: n_coeff,
            magnitude_spectrum_size,
            filter_bank: Self::new_mel_filter_bank(
                sample_rate,
                magnitude_spectrum_size,
                n_coeff,
                min_frequency,
                max_frequency,
            ),
            hamming_window: Self::new_hamming_window(samples_per_frame),
            sample_rate,
            fft,
            fft_buffer: vec![Complex32::default(); samples_per_frame],
            fft_scratch: vec![Complex32::default(); scratch_len],
            magnitude_spectrum: vec![0.0; magnitude_spectrum_size],
            mel_spectrum: vec![0.0; n_coeff],
            dct_cos_table: Self::new_dct_cos_table(n_coeff),
        }
    }
    pub fn set_out_size(&mut self, out_size: u16) {
        self.num_coefficients = (out_size + 1) as usize; // first coefficient is dropped
        let min_frequency = 0;
        let max_frequency = self.sample_rate / 2;
        self.filter_bank = Self::new_mel_filter_bank(
            self.sample_rate,
            self.magnitude_spectrum_size,
            self.num_coefficients,
            min_frequency,
            max_frequency,
        );
        self.mel_spectrum.resize(self.num_coefficients, 0.0);
        self.dct_cos_table = Self::new_dct_cos_table(self.num_coefficients);
        self.reset();
    }
    pub fn compute(&mut self, audio_samples: &[f32]) -> Vec<Vec<f32>> {
        audio_samples
            .chunks_exact(self.samples_per_shift)
            .filter_map(|audio_part| self.process_audio_part(audio_part))
            .collect()
    }
    pub fn reset(&mut self) {
        self.samples.clear();
    }
    fn process_audio_part(&mut self, audio_buffer: &[f32]) -> Option<Vec<f32>> {
        let shift_len = audio_buffer.len();
        if self.samples.len() >= self.samples_per_frame {
            self.samples.copy_within(shift_len.., 0);
            self.samples.truncate(self.samples.len() - shift_len);
            let mut tmp_sample = 0.;
            for &current in audio_buffer {
                let previous = tmp_sample;
                tmp_sample = current;
                self.samples
                    .push(tmp_sample - self.pre_emphasis_coefficient * previous);
            }
            Some(self.extract_mfccs_from_samples())
        } else {
            let mut tmp_sample = 0.;
            for &current in audio_buffer {
                let previous = tmp_sample;
                tmp_sample = current;
                self.samples
                    .push(tmp_sample - self.pre_emphasis_coefficient * previous);
            }
            None
        }
    }
    fn extract_mfccs_from_samples(&mut self) -> Vec<f32> {
        for i in 0..self.samples_per_frame {
            self.fft_buffer[i] = Complex32 {
                re: self.samples[i] * self.hamming_window[i],
                im: 0.,
            };
        }
        self.fft
            .process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);
        for i in 0..self.magnitude_spectrum_size {
            let c = self.fft_buffer[i];
            self.magnitude_spectrum[i] = (c.re * c.re + c.im * c.im).sqrt();
        }
        for i in 0..self.num_coefficients {
            let fb_row = &self.filter_bank[i];
            let mut sum = 0.0f32;
            for (j, &ms) in self.magnitude_spectrum.iter().enumerate() {
                sum += ms * ms * fb_row[j];
            }
            self.mel_spectrum[i] = (sum + f32::MIN_POSITIVE).ln();
        }
        let n_coeff = self.num_coefficients;
        let mut mfcc_frame = Vec::with_capacity(n_coeff.saturating_sub(1));
        for k in 1..n_coeff {
            let cos_row = &self.dct_cos_table[k * n_coeff..(k + 1) * n_coeff];
            let mut sum = 0.0f32;
            for (n, &mel) in self.mel_spectrum.iter().enumerate() {
                sum += mel * cos_row[n];
            }
            mfcc_frame.push(2. * sum);
        }
        mfcc_frame
    }

    //==================================================================
    // Feature extraction utils
    fn new_dct_cos_table(n_coeff: usize) -> Vec<f32> {
        if n_coeff == 0 {
            return Vec::new();
        }
        let pi_over_n = PI / n_coeff as f32;
        let mut table = Vec::with_capacity(n_coeff * n_coeff);
        for k in 0..n_coeff {
            for n in 0..n_coeff {
                table.push((pi_over_n * (n as f32 + 0.5) * k as f32).cos());
            }
        }
        table
    }
    fn new_hamming_window(samples_per_frame: usize) -> Vec<f32> {
        let ns_minus_1 = samples_per_frame - 1;
        (0..samples_per_frame)
            .map(|s| 0.54 - (0.46 * (2. * PI * (s as f32 / ns_minus_1 as f32)).cos()))
            .collect()
    }
    fn frequency_to_mel(frequency: usize) -> f32 {
        1127. * (1. + (frequency as f32 / 700.0)).ln()
    }
    fn new_mel_filter_bank(
        sample_rate: usize,
        magnitude_spectrum_size: usize,
        num_coefficients: usize,
        min_frequency: usize,
        max_frequency: usize,
    ) -> Vec<Vec<f32>> {
        let max_mel = Self::frequency_to_mel(max_frequency).floor();
        let min_mel = Self::frequency_to_mel(min_frequency).floor();
        let mut filter_bank = vec![vec![0.; magnitude_spectrum_size]; num_coefficients];
        let centre_indices: Vec<usize> = (0..num_coefficients + 2)
            .map(|i| {
                let f = i as f32 * (max_mel - min_mel) / (num_coefficients + 1) as f32 + min_mel;
                let mut tmp = (1_f32 + 1000.0 / 700.0).ln() / 1000.0;
                tmp = ((f * tmp).exp() - 1.) / (sample_rate as f32 / 2.);
                (0.5 + 700. * magnitude_spectrum_size as f32 * tmp).floor() as usize
            })
            .collect();
        for i in 0..num_coefficients {
            let filter_begin_index = centre_indices[i];
            let filter_center_index = centre_indices[i + 1];
            let filter_end_index = centre_indices[i + 2];
            let triangle_range_up = filter_center_index - filter_begin_index;
            let triangle_range_down = filter_end_index - filter_center_index;
            // upward slope
            for k in filter_begin_index..filter_center_index {
                filter_bank[i][k] = (k - filter_begin_index) as f32 / triangle_range_up as f32;
            }
            // downwards slope
            for k in filter_center_index..filter_end_index {
                filter_bank[i][k] = (filter_end_index - k) as f32 / triangle_range_down as f32;
            }
        }
        filter_bank
    }
}
