use crate::GainNormalizationConfig;

pub struct GainNormalizerFilter {
    window_size: usize,
    fixed_rms_level: bool,
    min_gain: f32,
    max_gain: f32,
    // state
    rms_level_ref: f32,
    rms_level_sqrt: f32,
    rms_level_window: Vec<f32>,
}
impl GainNormalizerFilter {
    pub fn filter(&mut self, signal: &mut [f32], rms_level: f32) -> f32 {
        if !self.rms_level_ref.is_nan() && rms_level != 0. {
            // update the window
            self.rms_level_window.push(rms_level);
            if self.rms_level_window.len() > self.window_size {
                self.rms_level_window.drain(0..1);
            }
            // calculate the mean of the rms window
            let frame_rms_level =
                self.rms_level_window.iter().sum::<f32>() / self.rms_level_window.len() as f32;
            // calculate the gain to apply
            let mut gain = self.rms_level_sqrt / frame_rms_level.sqrt();
            // range and round the value, trying to get a mostly uniform gain between frames.
            gain = ((gain * 10.).round() / 10.).clamp(self.min_gain, self.max_gain);
            // apply gain unless irrelevant
            if gain != 1. {
                for sample in signal {
                    *sample = (*sample * gain).clamp(-1., 1.);
                }
            }
            gain
        } else {
            1.
        }
    }
    pub fn get_rms_level_ref(&self) -> f32 {
        self.rms_level_ref
    }
    pub fn set_rms_level_ref(&mut self, rms_level: f32, window_size: usize) {
        if !self.fixed_rms_level {
            self.rms_level_ref = rms_level;
            self.rms_level_sqrt = rms_level.sqrt();
        }
        self.window_size = if window_size != 0 { window_size } else { 1 };
    }
    pub fn get_rms_level(signal: &[f32]) -> f32 {
        let mut sum_squared = 0.0;
        for sample in signal {
            sum_squared += sample * sample;
        }
        (sum_squared / signal.len() as f32).sqrt()
    }
    pub fn new(min_gain: f32, max_gain: f32, fixed_rms_level: Option<f32>) -> Self {
        Self {
            min_gain,
            max_gain,
            rms_level_ref: fixed_rms_level.unwrap_or(f32::NAN),
            rms_level_sqrt: fixed_rms_level.map(|s| s.sqrt()).unwrap_or(f32::NAN),
            fixed_rms_level: fixed_rms_level.is_some(),
            rms_level_window: Vec::new(),
            window_size: 1,
        }
    }
}
impl From<&GainNormalizationConfig> for Option<GainNormalizerFilter> {
    fn from(config: &GainNormalizationConfig) -> Self {
        if config.enabled {
            Some(GainNormalizerFilter::new(
                config.min_gain,
                config.max_gain,
                config.gain_ref,
            ))
        } else {
            None
        }
    }
}

#[test]
fn normalization_respects_gain_limit_and_zero_input() {
    let mut filter = GainNormalizerFilter::new(0.1, 0.8, Some(0.003));
    let mut samples: Vec<f32> = (0..480).map(|i| (i as f32 * 0.2).sin() * 0.2).collect();
    let peak = samples
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0_f32, f32::max);
    let rms = GainNormalizerFilter::get_rms_level(&samples);
    let gain = filter.filter(&mut samples, rms);
    assert!((0.1..=0.8).contains(&gain));
    assert!(samples
        .iter()
        .all(|x| x.is_finite() && x.abs() <= peak * 0.8 + 1e-6));
    let mut silence = vec![0.0; 480];
    filter.filter(&mut silence, 0.0);
    assert!(silence.iter().all(|x| *x == 0.0));
}
