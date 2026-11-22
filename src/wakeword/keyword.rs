//! Local personal keyword matching. Never falls back to energy activation.
use super::{KeywordObservation, WakeWordEngine};
use anyhow::{Context, Result};
use rustpotter::{PersonalDetectorConfig, Rustpotter, WakewordLoad, WakewordRef};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use tracing::{debug, info};

pub struct KeywordEngine {
    detector: Mutex<Option<Rustpotter>>,
    pending: VecDeque<u8>,
    frame_bytes: usize,
    last_observation: Option<KeywordObservation>,
}

impl KeywordEngine {
    #[allow(dead_code)]
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with_calibration(path, None)
    }

    pub fn load_with_calibration(
        path: &Path,
        cal: Option<&crate::config::WakewordCalibrationConfig>,
    ) -> Result<Self> {
        let reference =
            WakewordRef::load_from_file(path.to_str().context("Invalid keyword reference path")?)
                .map_err(anyhow::Error::msg)?;

        let calibration = resolve_detector_calibration(cal, |key| std::env::var(key).ok())?;
        let detector_cfg = calibration.to_detector_config();
        info!(
            threshold = detector_cfg.threshold,
            avg_threshold = detector_cfg.avg_threshold,
            min_scores = detector_cfg.min_scores,
            score_mode = ?detector_cfg.score_mode,
            eager = detector_cfg.eager,
            gain_norm = detector_cfg.gain_normalizer_enabled,
            band_pass = detector_cfg.band_pass_enabled,
            "Loaded personal wake-word detector with calibrated parameters"
        );

        let mut detector =
            rustpotter::personal_reference_detector_with_config(reference, &detector_cfg)
                .map_err(anyhow::Error::msg)?;

        // Support optional compound wake-word ("Штучка-Интеллект") alongside primary wake-word
        let compound_enabled = std::env::var("MINI_WAKEWORD_COMPOUND_ENABLED")
            .map(|v| v == "1")
            .unwrap_or(false)
            || std::env::var_os("MINI_WAKEWORD_COMPOUND_MODEL").is_some();

        if compound_enabled {
            let compound_path = std::env::var_os("MINI_WAKEWORD_COMPOUND_MODEL")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    path.parent()
                        .map(|p| p.join("alias-compound.rpw"))
                        .unwrap_or_else(|| {
                            std::path::PathBuf::from("/data/opt/mini-assistant/alias-compound.rpw")
                        })
                });

            if compound_path.exists() && compound_path != path {
                match WakewordRef::load_from_file(
                    compound_path
                        .to_str()
                        .context("Invalid compound reference path")?,
                ) {
                    Ok(compound_ref) => {
                        if let Err(e) =
                            detector.add_wakeword_ref("shtuchka-intellect", compound_ref)
                        {
                            tracing::warn!(
                                "Failed to add compound wake-word 'shtuchka-intellect': {e}"
                            );
                        } else {
                            info!(
                                path = ?compound_path,
                                "Loaded compound wake-word 'shtuchka-intellect' into detector"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Could not load compound wake-word reference: {e}");
                    }
                }
            }
        }
        let frame_bytes = detector.get_bytes_per_frame();
        anyhow::ensure!(
            (2..=8192).contains(&frame_bytes),
            "Invalid keyword frame geometry"
        );
        Ok(Self {
            detector: Mutex::new(Some(detector)),
            pending: VecDeque::new(),
            frame_bytes,
            last_observation: None,
        })
    }
}

/// Explicit settings are authoritative. Legacy environment settings are read only
/// when no persisted acoustic profile exists.
fn resolve_detector_calibration(
    saved: Option<&crate::config::WakewordCalibrationConfig>,
    mut env: impl FnMut(&str) -> Option<String>,
) -> Result<crate::config::WakewordCalibrationConfig> {
    if let Some(saved) = saved {
        saved.validate()?;
        return Ok(saved.clone());
    }
    let mut settings = crate::config::WakewordCalibrationConfig::default();
    for key in [
        "MINI_WAKEWORD_THRESHOLD",
        "MINI_WAKEWORD_AVG_THRESHOLD",
        "MINI_WAKEWORD_MIN_SCORES",
        "MINI_WAKEWORD_SCORE_MODE",
        "MINI_WAKEWORD_EAGER",
        "MINI_WAKEWORD_GAIN_NORM",
        "MINI_WAKEWORD_BAND_PASS",
    ] {
        let Some(value) = env(key) else { continue };
        match key {
            "MINI_WAKEWORD_THRESHOLD" => {
                settings.threshold = value.parse().context("Invalid legacy keyword threshold")?
            }
            "MINI_WAKEWORD_AVG_THRESHOLD" => {
                settings.avg_threshold = value
                    .parse()
                    .context("Invalid legacy keyword average threshold")?
            }
            "MINI_WAKEWORD_MIN_SCORES" => {
                settings.min_scores = value
                    .parse()
                    .context("Invalid legacy keyword score count")?
            }
            "MINI_WAKEWORD_SCORE_MODE" => settings.score_mode = value,
            _ => {
                let flag = match value.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => anyhow::bail!("Invalid legacy keyword boolean"),
                };
                match key {
                    "MINI_WAKEWORD_EAGER" => settings.eager = flag,
                    "MINI_WAKEWORD_GAIN_NORM" => settings.gain_normalizer = flag,
                    _ => settings.band_pass = flag,
                }
            }
        }
    }
    settings.validate()?;
    Ok(settings)
}

impl WakeWordEngine for KeywordEngine {
    fn update_calibration(&mut self, cfg: &PersonalDetectorConfig) {
        self.pending.clear();
        self.last_observation = None;
        if let Ok(Some(d)) = self.detector.get_mut() {
            d.update_personal_config(cfg);
            info!(
                threshold = cfg.threshold,
                avg_threshold = cfg.avg_threshold,
                min_scores = cfg.min_scores,
                ?cfg.score_mode,
                eager = cfg.eager,
                gain_norm = cfg.gain_normalizer_enabled,
                band_pass = cfg.band_pass_enabled,
                "Hot-updated personal wake-word detector calibration"
            );
        }
    }
    fn process(&mut self, pcm: &[u8]) -> bool {
        if pcm.len() > 8192 || !pcm.len().is_multiple_of(2) {
            self.reset();
            return false;
        }
        self.pending.extend(pcm);
        let detector = match self.detector.get_mut() {
            Ok(Some(d)) => d,
            _ => return false,
        };
        let mut detected = false;
        while self.pending.len() >= self.frame_bytes {
            let bytes: Vec<_> = self.pending.drain(..self.frame_bytes).collect();
            if let Some(detection) = detector.process_bytes(&bytes) {
                info!(
                    name = %detection.name,
                    score = detection.score,
                    avg_score = detection.avg_score,
                    counter = detection.counter,
                    gain = detection.gain,
                    "Wake-word detected! Opening voice pipeline"
                );
                self.last_observation = Some(KeywordObservation {
                    score: detection.score,
                    avg_score: detection.avg_score,
                    counter: detection.counter,
                    gain: detection.gain,
                    detected: true,
                });
                detected = true;
            } else if let Some(candidate) = detector.take_last_candidate() {
                if !detected {
                    self.last_observation = Some(KeywordObservation {
                        score: candidate.score,
                        avg_score: candidate.avg_score,
                        counter: candidate.counter,
                        gain: candidate.gain,
                        detected: false,
                    });
                }
                if candidate.score > 0.40 {
                    debug!(
                        name = %candidate.name,
                        candidate_score = candidate.score,
                        avg_score = candidate.avg_score,
                        gain = candidate.gain,
                        "KWS candidate score"
                    );
                }
            }
        }
        detected
    }

    fn take_observation(&mut self) -> Option<KeywordObservation> {
        self.last_observation.take()
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.last_observation = None;
        if let Ok(Some(d)) = self.detector.get_mut() {
            d.reset();
        }
    }
}

/// Missing/unvalidated enrollment means no acoustic activation.
pub struct DisabledKeyword;
impl WakeWordEngine for DisabledKeyword {
    fn process(&mut self, _: &[u8]) -> bool {
        false
    }
    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_invalid_reference_does_not_create_detector() {
        let absent = std::env::temp_dir().join(format!(
            "no-keyword-{}-{}.rpw",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        assert!(KeywordEngine::load(&absent).is_err());
        let file = absent.with_extension("invalid");
        std::fs::write(&file, b"not a personal reference").unwrap();
        let outcome = KeywordEngine::load(&file);
        std::fs::remove_file(file).unwrap();
        assert!(outcome.is_err());
    }

    #[test]
    fn unavailable_keyword_never_falls_back_to_loudness() {
        let mut keyword = DisabledKeyword;
        let loud: Vec<u8> = (0..512).flat_map(|_| i16::MAX.to_le_bytes()).collect();
        for _ in 0..100 {
            assert!(!keyword.process(&loud));
        }
        keyword.reset();
        assert!(!keyword.process(&loud));
    }

    #[test]
    fn persisted_acoustics_ignore_legacy_environment_overrides() {
        let saved = crate::config::WakewordCalibrationConfig {
            threshold: 0.57,
            min_scores: 11,
            ..Default::default()
        };
        let effective = resolve_detector_calibration(Some(&saved), |_| {
            panic!("Legacy environment must not be consulted for a persisted profile")
        })
        .unwrap();
        assert_eq!(effective, saved);
        let legacy = resolve_detector_calibration(None, |key| match key {
            "MINI_WAKEWORD_THRESHOLD" => Some("0.42".into()),
            "MINI_WAKEWORD_MIN_SCORES" => Some("9".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(legacy.threshold, 0.42);
        assert_eq!(legacy.min_scores, 9);
    }

    #[test]
    fn legacy_acoustics_do_not_silently_accept_invalid_values() {
        for invalid in ["NaN", "inf", "-1", "0.9", "garbage"] {
            assert!(resolve_detector_calibration(None, |key| {
                (key == "MINI_WAKEWORD_THRESHOLD").then(|| invalid.into())
            })
            .is_err());
        }
        assert!(resolve_detector_calibration(None, |key| {
            (key == "MINI_WAKEWORD_EAGER").then(|| "maybe".into())
        })
        .is_err());
    }
}
