mod audio;
mod config;
mod constants;
mod detector;
mod mfcc;
mod wakewords;
#[cfg(feature = "audio")]
pub use audio::{AudioEncoder, BandPassFilter, GainNormalizerFilter};
pub use audio::{Endianness, Sample, SampleFormat};
pub use config::{
    AudioFmt, BandPassConfig, DetectorConfig, FiltersConfig, GainNormalizationConfig,
    RustpotterConfig, ScoreMode, VADMode,
};
#[cfg(feature = "audio")]
pub use constants::{DETECTOR_INTERNAL_SAMPLE_RATE, MFCCS_EXTRACTOR_FRAME_LENGTH_MS};
pub use detector::{Rustpotter, RustpotterDetection};
pub use wakewords::{
    WakewordLoad, WakewordRef, WakewordRefBuildFromBuffers, WakewordRefBuildFromFiles, WakewordSave,
};

/// Configuration options for the personal reference detector.
#[derive(Debug, Clone)]
pub struct PersonalDetectorConfig {
    pub threshold: f32,
    pub avg_threshold: f32,
    pub min_scores: usize,
    pub score_mode: ScoreMode,
    pub gain_normalizer_enabled: bool,
    pub band_pass_enabled: bool,
}

impl Default for PersonalDetectorConfig {
    fn default() -> Self {
        Self {
            threshold: 0.48,
            avg_threshold: 0.25,
            min_scores: 2,
            score_mode: ScoreMode::Max,
            gain_normalizer_enabled: true,
            band_pass_enabled: false,
        }
    }
}

/// Construct the Mini's personal MFCC/DTW matcher with custom or default detector parameters.
pub fn personal_reference_detector_with_config(
    mut reference: WakewordRef,
    detector_cfg: &PersonalDetectorConfig,
) -> Result<Rustpotter, String> {
    reference.validate_personal_reference()?;
    reference.threshold = None;
    reference.avg_threshold = None;
    let mut config = RustpotterConfig::default();
    config.fmt.sample_rate = 16_000;
    config.fmt.sample_format = SampleFormat::I16;
    config.fmt.channels = 1;
    config.detector.threshold = detector_cfg.threshold;
    config.detector.avg_threshold = detector_cfg.avg_threshold;
    config.detector.min_scores = detector_cfg.min_scores;
    config.detector.score_mode = detector_cfg.score_mode;
    config.filters.gain_normalizer.enabled = detector_cfg.gain_normalizer_enabled;
    config.filters.band_pass.enabled = detector_cfg.band_pass_enabled;
    let mut detector = Rustpotter::new(&config)?;
    detector.add_wakeword_ref("personal-keyword", reference)?;
    Ok(detector)
}

/// Construct the Mini's personal MFCC/DTW matcher with default parameters.
pub fn personal_reference_detector(reference: WakewordRef) -> Result<Rustpotter, String> {
    personal_reference_detector_with_config(reference, &PersonalDetectorConfig::default())
}
