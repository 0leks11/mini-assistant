//! Local personal keyword matching. Never falls back to energy activation.
use super::WakeWordEngine;
use anyhow::{Context, Result};
use rustpotter::{PersonalDetectorConfig, Rustpotter, ScoreMode, WakewordLoad, WakewordRef};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use tracing::{debug, info};

pub struct KeywordEngine {
    detector: Mutex<Option<Rustpotter>>,
    pending: VecDeque<u8>,
    frame_bytes: usize,
}

impl KeywordEngine {
    pub fn load(path: &Path) -> Result<Self> {
        let reference =
            WakewordRef::load_from_file(path.to_str().context("Invalid keyword reference path")?)
                .map_err(anyhow::Error::msg)?;

        let threshold = std::env::var("MINI_WAKEWORD_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && (0.1..=1.0).contains(v))
            .unwrap_or(0.42);

        let avg_threshold = std::env::var("MINI_WAKEWORD_AVG_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
            .unwrap_or(0.22);

        let min_scores = std::env::var("MINI_WAKEWORD_MIN_SCORES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| (1..=20).contains(v))
            .unwrap_or(2);

        let score_mode = std::env::var("MINI_WAKEWORD_SCORE_MODE")
            .ok()
            .and_then(|v| parse_score_mode(&v))
            .unwrap_or(ScoreMode::Max);

        let detector_cfg = PersonalDetectorConfig {
            threshold,
            avg_threshold,
            min_scores,
            score_mode,
            gain_normalizer_enabled: true,
            band_pass_enabled: false,
        };

        info!(
            threshold,
            avg_threshold,
            min_scores,
            ?score_mode,
            "Loaded personal wake-word detector with calibrated parameters"
        );

        let mut detector =
            rustpotter::personal_reference_detector_with_config(reference, &detector_cfg)
                .map_err(anyhow::Error::msg)?;

        // Support compound wake-word ("Штучка-Интеллект") alongside primary "Штучка"
        let compound_path = std::env::var_os("MINI_WAKEWORD_COMPOUND_MODEL")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                path.parent()
                    .map(|p| p.join("alias-compound.rpw"))
                    .unwrap_or_else(|| {
                        std::path::PathBuf::from("/data/opt/mini-assistant/alias-compound.rpw")
                    })
            });

        if compound_path.exists() {
            match WakewordRef::load_from_file(
                compound_path
                    .to_str()
                    .context("Invalid compound reference path")?,
            ) {
                Ok(compound_ref) => {
                    if let Err(e) = detector.add_wakeword_ref("shtuchka-intellect", compound_ref) {
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
        let frame_bytes = detector.get_bytes_per_frame();
        anyhow::ensure!(
            (2..=8192).contains(&frame_bytes),
            "Invalid keyword frame geometry"
        );
        Ok(Self {
            detector: Mutex::new(Some(detector)),
            pending: VecDeque::new(),
            frame_bytes,
        })
    }
}

fn parse_score_mode(s: &str) -> Option<ScoreMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "max" => Some(ScoreMode::Max),
        "median" | "p50" => Some(ScoreMode::Median),
        "average" | "avg" => Some(ScoreMode::Average),
        "p75" => Some(ScoreMode::P75),
        "p80" => Some(ScoreMode::P80),
        "p90" => Some(ScoreMode::P90),
        "p95" => Some(ScoreMode::P95),
        _ => None,
    }
}

impl WakeWordEngine for KeywordEngine {
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
                detected = true;
            } else if let Some(candidate) = detector.take_last_candidate() {
                if candidate.score > 0.38 {
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

    fn reset(&mut self) {
        self.pending.clear();
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
}
