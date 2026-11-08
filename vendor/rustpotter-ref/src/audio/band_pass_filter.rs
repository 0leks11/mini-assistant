use std::f32::consts::PI;

use crate::{constants::DETECTOR_INTERNAL_SAMPLE_RATE, BandPassConfig};

pub struct BandPassFilter {
    // options
    a0: f32,
    a1: f32,
    a2: f32,
    b1: f32,
    b2: f32,
    // state
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}
impl BandPassFilter {
    pub fn filter(&mut self, signal: &mut [f32]) {
        for sample in signal.iter_mut() {
            let x = *sample;
            *sample = self.a0 * x + self.a1 * self.x1 + self.a2 * self.x2
                - self.b1 * self.y1
                - self.b2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = *sample;
        }
    }
    pub fn new(sample_rate: f32, low_cutoff: f32, high_cutoff: f32) -> Self {
        let omega_low = 2.0 * PI * low_cutoff / sample_rate;
        let omega_high = 2.0 * PI * high_cutoff / sample_rate;
        let cos_omega_low = omega_low.cos();
        let cos_omega_high = omega_high.cos();
        let alpha_low = omega_low.sin() / 2.0;
        let alpha_high = omega_high.sin() / 2.0;
        let a0 = 1.0 / (1.0 + alpha_high - alpha_low);
        let a1 = -2.0 * cos_omega_low * a0;
        let a2 = (1.0 - alpha_high - alpha_low) * a0;
        let b1 = -2.0 * cos_omega_high * a0;
        let b2 = (1.0 - alpha_high + alpha_low) * a0;
        Self {
            a0,
            a1,
            a2,
            b1,
            b2,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }
}
impl From<&BandPassConfig> for Option<BandPassFilter> {
    fn from(config: &BandPassConfig) -> Self {
        if config.enabled {
            Some(BandPassFilter::new(
                DETECTOR_INTERNAL_SAMPLE_RATE as f32,
                config.low_cutoff,
                config.high_cutoff,
            ))
        } else {
            None
        }
    }
}

#[test]
fn silence_remains_silence() {
    let mut filter = BandPassFilter::new(16_000.0, 80.0, 400.0);
    let mut samples = vec![0.0; 960];
    filter.filter(&mut samples);
    assert!(samples.iter().all(|x| *x == 0.0));
}

#[test]
fn chunked_filter_preserves_continuous_state() {
    let signal: Vec<f32> = (0..1920).map(|i| (i as f32 * 0.17).sin() * 0.02).collect();
    let mut whole = signal.clone();
    let mut chunked = signal;
    BandPassFilter::new(16_000.0, 80.0, 400.0).filter(&mut whole);
    let mut filter = BandPassFilter::new(16_000.0, 80.0, 400.0);
    for chunk in chunked.chunks_mut(137) {
        filter.filter(chunk);
    }
    assert!(chunked.iter().all(|x| x.is_finite()));
    assert_eq!(whole, chunked);
}
