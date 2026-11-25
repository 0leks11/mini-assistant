use std::collections::HashMap;

use crate::{
    mfcc::MfccComparator, wakewords::WakewordDetector, RustpotterDetection, ScoreMode, WakewordRef,
};

#[derive(Default)]
struct NormScratch {
    data: Vec<f32>,
    mean: Vec<f32>,
}

impl NormScratch {
    fn normalize_prefix(&mut self, frames: &[Vec<f32>], max_len: usize) -> (usize, usize) {
        let num_frames = frames.len().min(max_len);
        if num_frames == 0 {
            return (0, 0);
        }
        let dim = frames[0].len();
        self.mean.resize(dim, 0.0);
        self.mean.fill(0.0);
        self.data.resize(num_frames * dim, 0.0);

        for (i, frame) in frames.iter().take(num_frames).enumerate() {
            let base = i * dim;
            for (j, &val) in frame.iter().take(dim).enumerate() {
                self.data[base + j] = val;
                self.mean[j] += val;
            }
        }
        let inv = 1.0 / num_frames as f32;
        for row in self.data.chunks_exact_mut(dim).take(num_frames) {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell -= self.mean[j] * inv;
            }
        }
        (num_frames, dim)
    }
}

pub(crate) struct WakewordComparator {
    name: String,
    avg_features: Option<Vec<Vec<f32>>>,
    samples_features: Vec<(String, Vec<Vec<f32>>)>,
    threshold: Option<f32>,
    avg_threshold: Option<f32>,
    rms_level: f32,
    // state & reusable scratch buffers
    score_mode: ScoreMode,
    mfcc_comparator: MfccComparator,
    norm_scratch: NormScratch,
    scratch_scores: Vec<f32>,
    scratch_named_scores: Vec<(String, f32)>,
}

impl WakewordComparator {
    fn get_percentile(&self, sorted_values: &[f32], percentile: f32) -> f32 {
        let n = sorted_values.len();
        let index = percentile / 100.0 * (n - 1) as f32;
        let index_floor = index.floor();
        if index_floor == index {
            sorted_values[index as usize]
        } else {
            let i = index_floor as usize;
            let d = index - index_floor;
            sorted_values[i] * (1.0 - d) + sorted_values[i + 1] * d
        }
    }
    pub fn new(
        wakeword: &WakewordRef,
        mfcc_comparator: MfccComparator,
        score_mode: ScoreMode,
    ) -> Self {
        let mut samples_features: Vec<(String, Vec<Vec<f32>>)> = wakeword
            .samples_features
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        samples_features.sort_by_key(|(_, v)| v.len());
        let cap = samples_features.len();
        WakewordComparator {
            name: wakeword.name.clone(),
            avg_features: wakeword.avg_features.clone(),
            samples_features,
            threshold: wakeword.threshold,
            avg_threshold: wakeword.avg_threshold,
            rms_level: wakeword.rms_level,
            score_mode,
            mfcc_comparator,
            norm_scratch: NormScratch::default(),
            scratch_scores: Vec::with_capacity(cap),
            scratch_named_scores: Vec::with_capacity(cap),
        }
    }
}

impl WakewordDetector for WakewordComparator {
    fn get_mfcc_frame_size(&self) -> usize {
        self.samples_features
            .iter()
            .map(|(_, v)| v.len())
            .max()
            .unwrap_or(usize::MIN)
    }

    fn run_detection(
        &mut self,
        mfcc_frame: &[Vec<f32>],
        avg_threshold: f32,
        threshold: f32,
    ) -> Option<RustpotterDetection> {
        let avg_threshold = self.avg_threshold.unwrap_or(avg_threshold);
        let mut avg_score = 0.;
        let mut cached_len: Option<(usize, usize, usize)> = None;

        if let Some(ref wakeword_samples_avg_features) = self.avg_features {
            if avg_threshold != 0. {
                let target_len = wakeword_samples_avg_features.len();
                let (q_frames, dim) = self.norm_scratch.normalize_prefix(mfcc_frame, target_len);
                cached_len = Some((target_len, q_frames, dim));
                avg_score = self.mfcc_comparator.compare_flat(
                    wakeword_samples_avg_features,
                    &self.norm_scratch.data[..q_frames * dim],
                    q_frames,
                    dim,
                );
                if avg_score < avg_threshold {
                    return None;
                }
            }
        }
        let threshold = self.threshold.unwrap_or(threshold);
        self.scratch_scores.clear();
        self.scratch_named_scores.clear();
        for (name, wakeword_sample_features) in &self.samples_features {
            let target_len = wakeword_sample_features.len();
            let (q_frames, dim) = match cached_len {
                Some((prev_target, qf, d)) if prev_target == target_len => (qf, d),
                _ => {
                    let (qf, d) = self.norm_scratch.normalize_prefix(mfcc_frame, target_len);
                    cached_len = Some((target_len, qf, d));
                    (qf, d)
                }
            };
            let s = self.mfcc_comparator.compare_flat(
                wakeword_sample_features,
                &self.norm_scratch.data[..q_frames * dim],
                q_frames,
                dim,
            );
            self.scratch_scores.push(s);
            self.scratch_named_scores.push((name.clone(), s));
        }

        let score = match self.score_mode {
            ScoreMode::Average => {
                self.scratch_scores.iter().sum::<f32>() / self.scratch_scores.len() as f32
            }
            ScoreMode::Max => {
                self.scratch_scores.sort_by(|a, b| b.total_cmp(a));
                self.scratch_scores[0]
            }
            ScoreMode::Median | ScoreMode::P50 => {
                self.scratch_scores.sort_by(|a, b| a.total_cmp(b));
                self.get_percentile(&self.scratch_scores, 50.)
            }
            ScoreMode::P25 => {
                self.scratch_scores.sort_by(|a, b| a.total_cmp(b));
                self.get_percentile(&self.scratch_scores, 25.)
            }
            ScoreMode::P75 => {
                self.scratch_scores.sort_by(|a, b| a.total_cmp(b));
                self.get_percentile(&self.scratch_scores, 75.)
            }
            ScoreMode::P80 => {
                self.scratch_scores.sort_by(|a, b| a.total_cmp(b));
                self.get_percentile(&self.scratch_scores, 80.)
            }
            ScoreMode::P90 => {
                self.scratch_scores.sort_by(|a, b| a.total_cmp(b));
                self.get_percentile(&self.scratch_scores, 90.)
            }
            ScoreMode::P95 => {
                self.scratch_scores.sort_by(|a, b| a.total_cmp(b));
                self.get_percentile(&self.scratch_scores, 95.)
            }
        };
        if score > threshold {
            let scores: HashMap<String, f32> = self.scratch_named_scores.drain(..).collect();
            Some(RustpotterDetection {
                name: self.name.clone(),
                avg_score,
                score,
                scores,
                counter: usize::MIN, // added by the detector
                gain: f32::NAN,      // added by the detector
            })
        } else {
            None
        }
    }

    fn get_rms_level(&self) -> f32 {
        self.rms_level
    }

    fn get_mfcc_size(&self) -> u16 {
        self.samples_features
            .first()
            .map(|(_, v)| v[0].len() as u16)
            .unwrap_or(0)
    }

    fn update_config(&mut self, score_ref: f32, band_size: u16, score_mode: ScoreMode) {
        self.score_mode = score_mode;
        self.mfcc_comparator = MfccComparator::new(score_ref, band_size);
    }
}
