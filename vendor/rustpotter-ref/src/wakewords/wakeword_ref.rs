use super::{comp::WakewordComparator, WakewordDetector, WakewordFile};
use crate::{
    mfcc::MfccComparator,
    wakewords::{WakewordLoad, WakewordSave},
    ScoreMode, WakewordRefBuildFromBuffers, WakewordRefBuildFromFiles,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;

/// Wakeword representation.
#[derive(Serialize, Deserialize)]
pub struct WakewordRef {
    pub name: String,
    pub avg_features: Option<Vec<Vec<f32>>>,
    pub samples_features: HashMap<String, Vec<Vec<f32>>>,
    pub threshold: Option<f32>,
    pub avg_threshold: Option<f32>,
    pub rms_level: f32,
    pub mfcc_size: u16,
}
impl WakewordLoad for WakewordRef {
    fn load_from_file(path: &str) -> Result<Self, String> {
        let path = std::path::Path::new(path);
        if !path.is_file() {
            return Err("Personal reference must be a regular file".into());
        }
        let file = std::fs::File::open(path).map_err(|_| "Personal reference unavailable")?;
        if !file
            .metadata()
            .map_err(|_| "Reference metadata unavailable")?
            .is_file()
        {
            return Err("Personal reference must be a regular file".into());
        }
        let mut bytes = Vec::new();
        file.take((super::bounded::MAX_REFERENCE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "Personal reference read failed")?;
        Self::load_from_buffer(&bytes)
    }

    fn load_from_buffer(buffer: &[u8]) -> Result<Self, String> {
        super::bounded::preflight_cbor(buffer)?;
        let reference: Self =
            ciborium::de::from_reader(buffer).map_err(|_| "Invalid personal MFCC/DTW reference")?;
        reference.validate_personal_reference()?;
        Ok(reference)
    }
}
impl WakewordSave for WakewordRef {}
impl WakewordRefBuildFromBuffers for WakewordRef {}
impl WakewordRefBuildFromFiles for WakewordRef {}
impl WakewordFile for WakewordRef {
    fn get_detector(
        &self,
        score_ref: f32,
        band_size: u16,
        score_mode: ScoreMode,
    ) -> Box<dyn WakewordDetector> {
        Box::new(WakewordComparator::new(
            self,
            MfccComparator::new(score_ref, band_size),
            score_mode,
        ))
    }

    fn get_mfcc_size(&self) -> u16 {
        self.mfcc_size
    }
}
impl WakewordRef {
    /// Validate the bounded personal reference format supported by the Mini adaptation.
    /// This is shape/numeric validation, not proof of acoustic recognition quality.
    pub fn validate_personal_reference(&self) -> Result<(), String> {
        if self.name.is_empty() || self.name.len() > 64 || self.mfcc_size != 16 {
            return Err("Invalid personal reference identity or MFCC dimension".into());
        }
        if !(3..=8).contains(&self.samples_features.len()) {
            return Err("A personal reference requires 3 to 8 templates".into());
        }
        if !self.rms_level.is_finite()
            || !(0.0..=1.0).contains(&self.rms_level)
            || self.rms_level == 0.0
        {
            return Err("Invalid personal reference RMS".into());
        }
        for threshold in [self.threshold, self.avg_threshold].into_iter().flatten() {
            if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                return Err("Invalid personal reference threshold".into());
            }
        }
        let valid_matrix = |matrix: &[Vec<f32>]| {
            (20..=300).contains(&matrix.len())
                && matrix.iter().all(|row| {
                    row.len() == 16 && row.iter().all(|v| v.is_finite() && v.abs() <= 10_000.0)
                })
                && matrix.iter().flatten().any(|v| v.abs() > 1e-6)
        };
        for (name, matrix) in &self.samples_features {
            if name.is_empty() || name.len() > 128 || !valid_matrix(matrix) {
                return Err("Invalid personal reference template".into());
            }
        }
        let avg = self
            .avg_features
            .as_ref()
            .ok_or("Personal reference average missing")?;
        if !valid_matrix(avg)
            || avg.len()
                != self
                    .samples_features
                    .values()
                    .map(Vec::len)
                    .max()
                    .unwrap_or(0)
        {
            return Err("Invalid personal reference average".into());
        }
        Ok(())
    }

    pub(crate) fn new(
        name: String,
        threshold: Option<f32>,
        avg_threshold: Option<f32>,
        avg_features: Option<Vec<Vec<f32>>>,
        rms_level: f32,
        samples_features: HashMap<String, Vec<Vec<f32>>>,
    ) -> Result<WakewordRef, String> {
        let mfcc_size = samples_features
            .values()
            .next()
            .and_then(|m| m.first())
            .map(Vec::len)
            .ok_or("Empty personal reference template")? as u16;
        let result = WakewordRef {
            name,
            threshold,
            avg_threshold,
            avg_features,
            samples_features,
            rms_level,
            mfcc_size,
        };
        result.validate_personal_reference()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_reference() -> WakewordRef {
        let matrix: Vec<Vec<f32>> = (0..40)
            .map(|i| (0..16).map(|j| ((i + j) as f32 * 0.3).sin()).collect())
            .collect();
        WakewordRef {
            name: "personal".into(),
            avg_features: Some(matrix.clone()),
            samples_features: (0..3)
                .map(|i| (format!("sample-{i}"), matrix.clone()))
                .collect(),
            threshold: Some(0.65),
            avg_threshold: Some(0.60),
            rms_level: 0.01,
            mfcc_size: 16,
        }
    }

    #[test]
    fn bounded_reference_round_trip_and_detector_creation() {
        let reference = valid_reference();
        let bytes = reference.save_to_buffer().unwrap();
        let loaded = WakewordRef::load_from_buffer(&bytes).unwrap();
        assert_eq!(loaded.samples_features.len(), 3);
        assert!(crate::personal_reference_detector(loaded).is_ok());
        let mut trailing = bytes;
        trailing.push(0xf6);
        assert!(WakewordRef::load_from_buffer(&trailing).is_err());
    }

    #[test]
    fn reference_rejects_nonfinite_shape_and_template_count() {
        let mut reference = valid_reference();
        reference.samples_features.values_mut().next().unwrap()[0][0] = f32::NAN;
        assert!(WakewordRef::load_from_buffer(&reference.save_to_buffer().unwrap()).is_err());
        let mut reference = valid_reference();
        reference.samples_features.remove("sample-0");
        assert!(reference.validate_personal_reference().is_err());
        let mut reference = valid_reference();
        reference.avg_features.as_mut().unwrap()[0].pop();
        assert!(reference.validate_personal_reference().is_err());
        assert!(WakewordRef::load_from_buffer(&vec![0; 1024 * 1024 + 1]).is_err());
    }

    #[test]
    fn malformed_enrollment_returns_error_instead_of_panicking() {
        let samples = (0..3)
            .map(|i| (format!("sample-{i}"), b"invalid WAV".to_vec()))
            .collect();
        assert!(
            WakewordRef::new_from_sample_buffers("personal".into(), None, None, samples, 16)
                .is_err()
        );
        assert!(
            WakewordRef::new_from_sample_files("personal".into(), None, None, Vec::new(), 16)
                .is_err()
        );
    }
}
