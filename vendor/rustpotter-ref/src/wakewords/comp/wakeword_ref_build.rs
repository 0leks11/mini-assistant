use std::{cmp::Ordering, collections::HashMap, fs::File, io::BufReader, path::Path};

use crate::{
    mfcc::{MfccAverager, MfccWavFileExtractor},
    WakewordRef,
};

pub trait WakewordRefBuildFromBuffers {
    fn new_from_sample_buffers(
        name: String,
        threshold: Option<f32>,
        avg_threshold: Option<f32>,
        samples: HashMap<String, Vec<u8>>,
        mfcc_size: u16,
    ) -> Result<WakewordRef, String> {
        if !(3..=8).contains(&samples.len()) || mfcc_size != 16 {
            return Err("Personal enrollment needs 3 to 8 samples and 16 MFCC coefficients".into());
        }
        let mut samples_features: HashMap<String, Vec<Vec<f32>>> = HashMap::new();
        let mut rms_level = 0.;
        for (key, buffer) in samples {
            if buffer.is_empty() || buffer.len() > 128 * 1024 {
                return Err("Enrollment WAV size outside limit".into());
            }
            let mut sample_rms_level = 0.;
            let features = MfccWavFileExtractor::compute_mfccs(
                BufReader::new(buffer.as_slice()),
                &mut sample_rms_level,
                mfcc_size,
            )?;
            // Reject degenerate/nonfinite samples before DTW averaging: NaN path costs
            // are not valid inputs to the upstream optimal-path traversal.
            if !sample_rms_level.is_finite()
                || sample_rms_level <= 0.0
                || !(20..=300).contains(&features.len())
                || !features.iter().all(|row| {
                    row.len() == 16 && row.iter().all(|v| v.is_finite() && v.abs() <= 10_000.0)
                })
                || !features.iter().flatten().any(|v| v.abs() > 1e-6)
            {
                return Err("Degenerate or invalid enrollment features".into());
            }
            samples_features.insert(key, features);
            if sample_rms_level > rms_level {
                rms_level = sample_rms_level;
            }
        }
        WakewordRef::new(
            name,
            threshold,
            avg_threshold,
            compute_avg_samples_features(&samples_features),
            rms_level,
            samples_features,
        )
    }
}
pub trait WakewordRefBuildFromFiles {
    fn new_from_sample_files(
        name: String,
        threshold: Option<f32>,
        avg_threshold: Option<f32>,
        samples: Vec<String>,
        mfcc_size: u16,
    ) -> Result<WakewordRef, String> {
        if !(3..=8).contains(&samples.len()) || mfcc_size != 16 {
            return Err("Personal enrollment needs 3 to 8 samples and 16 MFCC coefficients".into());
        }
        let mut buffers = HashMap::new();
        for (index, sample_path) in samples.into_iter().enumerate() {
            let path = Path::new(&sample_path);
            if !path.exists() || !path.is_file() {
                return Err("File not found: ".to_owned() + &sample_path);
            }
            let file = match File::open(&sample_path) {
                Ok(it) => it,
                Err(err) => {
                    return Err("Unable to open file ".to_owned()
                        + &sample_path
                        + ": "
                        + &err.to_string())
                }
            };
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(128 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "Enrollment WAV read failed")?;
            if bytes.len() > 128 * 1024 {
                return Err("Enrollment WAV size outside limit".into());
            }
            buffers.insert(format!("sample-{index:02}"), bytes);
        }
        WakewordRef::new_from_sample_buffers(name, threshold, avg_threshold, buffers, mfcc_size)
    }
}

fn compute_avg_samples_features(
    templates: &HashMap<String, Vec<Vec<f32>>>,
) -> Option<Vec<Vec<f32>>> {
    if templates.len() <= 1 {
        return None;
    }
    let mut template_values: Vec<_> = templates.iter().collect();
    template_values.sort_by(|a, b| {
        let equality = b.1.len().cmp(&a.1.len());
        if equality == Ordering::Equal {
            a.0.cmp(b.0)
        } else {
            equality
        }
    });
    let template_vec = template_values
        .iter()
        .map(|(_, sample)| sample.to_vec())
        .collect::<Vec<Vec<Vec<f32>>>>();
    MfccAverager::average(template_vec)
}
