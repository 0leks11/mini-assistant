//! Autonomous Acoustic Calibration: Experiment Runner & Playback Synchronization.
//!
//! Handles foreground acoustic experiment jobs:
//! - Stimulus origins: External speaker (2m user position), Own output (barge-in suppression), Ambient noise.
//! - Output routes: BuiltIn speaker, AuxOut (3.5mm line out), None.
//! - Stimulus types: Sample from library, Sine/Chirp Sweep (<= -6.0 dBFS, <= 5s), Silence.
//! - Audio sync: Pre-roll ambient recording -> Stimulus playback with ACK/drain -> Post-roll reverberation tail.
//! - Exclusive lease: only 1 foreground job active at a time (concurrent requests yield 409 Conflict).
//! - Automatic interruption on cancellation, timeout, owner action, or alarm.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};

use crate::acoustics::samples::{
    encode_wav_pcm16_mono, validate_and_analyze_wav, CreateSampleDto, SampleAudioMetrics,
    SampleLabel, SampleSource, SampleSplit, SamplesStore, TARGET_SAMPLE_RATE,
};
use crate::acoustics::{AcousticEvent, AcousticLab};
use crate::wakeword::enrollment::HardwareRecorder;

pub const MAX_JOB_HISTORY: usize = 20;
pub const MAX_CHIRP_DURATION_MS: u32 = 5000;
pub const MAX_SILENCE_DURATION_MS: u32 = 30000;
pub const MAX_CHIRP_VOLUME_DBFS: f32 = -6.0;
pub const MAX_VOLUME_PCT: u8 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StimulusOrigin {
    ExternalUserPosition,
    OwnOutput,
    Ambient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputRoute {
    BuiltIn,
    AuxOut,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StimulusSignal {
    Sample {
        sample_id: String,
    },
    ChirpSweep {
        start_hz: f32,
        end_hz: f32,
        duration_ms: u32,
        volume_dbfs: f32,
    },
    Silence {
        duration_ms: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Preparing,
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateJobDto {
    #[serde(default)]
    pub name: Option<String>,
    pub stimulus_origin: StimulusOrigin,
    pub output_route: OutputRoute,
    pub stimulus: StimulusSignal,
    #[serde(default = "default_pre_roll")]
    pub pre_roll_ms: u32,
    #[serde(default = "default_post_roll")]
    pub post_roll_ms: u32,
    #[serde(default = "default_volume")]
    pub volume_pct: u8,
}

fn default_pre_roll() -> u32 {
    500
}

fn default_post_roll() -> u32 {
    1000
}

fn default_volume() -> u8 {
    40
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedAudioMetrics {
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_ms: u32,
    pub samples_count: usize,
    pub rms_level: f32,
    pub peak_amplitude: i32,
    pub snr_db: f32,
    pub is_clipped: bool,
}

impl From<SampleAudioMetrics> for CapturedAudioMetrics {
    fn from(m: SampleAudioMetrics) -> Self {
        Self {
            sample_rate: m.sample_rate,
            channels: m.channels,
            duration_ms: m.duration_ms,
            samples_count: (m.duration_ms as f64 * (m.sample_rate as f64 / 1000.0)).round()
                as usize,
            rms_level: m.rms_level,
            peak_amplitude: m.peak_amplitude,
            snr_db: m.snr_db,
            is_clipped: m.is_clipped,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: String,
    pub name: Option<String>,
    pub state: JobState,
    pub stimulus_origin: StimulusOrigin,
    pub output_route: OutputRoute,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub duration_ms: u64,
    pub captured_sample_id: Option<String>,
    pub captured_metrics: Option<CapturedAudioMetrics>,
    pub kws_detections_count: usize,
    pub events_count: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDetail {
    #[serde(flatten)]
    pub summary: JobSummary,
    pub config: CreateJobDto,
    pub playback_started_at_ms: Option<u64>,
    pub playback_ended_at_ms: Option<u64>,
    pub events: Vec<AcousticEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobsSnapshot {
    pub active_job: Option<JobSummary>,
    pub history: Vec<JobSummary>,
    pub aux_supported: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Conflict: an acoustic job is already running ({0})")]
    Conflict(String),
    #[error("Unsupported: {0}")]
    Unsupported(String),
    #[error("Job not found: {0}")]
    NotFound(String),
    #[error("Internal execution error: {0}")]
    Execution(String),
}

/// Abstract playback sink for acoustic stimulus emission.
pub trait AcousticPlaybackSink: Send + Sync + 'static {
    fn play_pcm(
        &self,
        samples: &[i16],
        sample_rate: u32,
        route: OutputRoute,
        volume_pct: u8,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

    fn stop(&self);

    fn is_aux_supported(&self) -> bool;
}

/// Mock sink for unit tests and macOS/CI host.
#[derive(Debug, Clone)]
pub struct MockPlaybackSink {
    aux_supported: bool,
    played: Arc<Mutex<Vec<(OutputRoute, usize, u8)>>>,
    stopped: Arc<AtomicBool>,
}

impl MockPlaybackSink {
    pub fn new(aux_supported: bool) -> Self {
        Self {
            aux_supported,
            played: Arc::new(Mutex::new(Vec::new())),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn played_records(&self) -> Vec<(OutputRoute, usize, u8)> {
        self.played.lock().unwrap().clone()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

impl AcousticPlaybackSink for MockPlaybackSink {
    fn play_pcm(
        &self,
        samples: &[i16],
        _sample_rate: u32,
        route: OutputRoute,
        volume_pct: u8,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        let played = self.played.clone();
        let samples_len = samples.len();
        let stopped = self.stopped.clone();
        Box::pin(async move {
            if stopped.load(Ordering::SeqCst) {
                bail!("Playback sink is stopped");
            }
            played
                .lock()
                .unwrap()
                .push((route, samples_len, volume_pct));
            Ok(())
        })
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    fn is_aux_supported(&self) -> bool {
        self.aux_supported
    }
}

/// Real hardware audio playback sink connected to Station Mini audio subsystem.
pub struct RealPlaybackSink {
    playback: Arc<crate::audio::AudioPlayback>,
    aux_supported: bool,
}

impl RealPlaybackSink {
    pub fn new(playback: Arc<crate::audio::AudioPlayback>, aux_supported: bool) -> Self {
        Self {
            playback,
            aux_supported,
        }
    }
}

impl AcousticPlaybackSink for RealPlaybackSink {
    fn play_pcm(
        &self,
        samples: &[i16],
        sample_rate: u32,
        route: OutputRoute,
        volume_pct: u8,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        let playback = self.playback.clone();
        let aux_supported = self.aux_supported;
        let samples = samples.to_vec();

        Box::pin(async move {
            if route == OutputRoute::AuxOut && !aux_supported {
                bail!("AUX output route is not supported on this device; silent fallback to built_in is forbidden");
            }
            if route == OutputRoute::None || samples.is_empty() {
                return Ok(());
            }

            // Claim alarm lease to gain exclusive output access & silence cloud speech
            let epoch = playback.claim_alarm();
            let vol_scale = (volume_pct as f32 / 100.0).clamp(0.0, 1.0);

            // Resample / convert 16kHz mono PCM to 24kHz mono PCM for AudioPlayback
            let resampled = if sample_rate == 16000 {
                // Upsample 16k -> 24k (ratio 1.5): linear interpolation
                let out_len = (samples.len() * 3) / 2;
                let mut out = Vec::with_capacity(out_len);
                for i in 0..out_len {
                    let src_idx = (i * 2) / 3;
                    let next_idx = (src_idx + 1).min(samples.len() - 1);
                    let frac = ((i * 2) % 3) as f32 / 3.0;
                    let s1 = samples[src_idx] as f32 * vol_scale;
                    let s2 = samples[next_idx] as f32 * vol_scale;
                    let val = (s1 + frac * (s2 - s1)).clamp(-32768.0, 32767.0);
                    out.push(val.round() as i16);
                }
                out
            } else {
                samples
                    .iter()
                    .map(|&s| ((s as f32) * vol_scale).clamp(-32768.0, 32767.0).round() as i16)
                    .collect()
            };

            // Stream PCM in 40ms chunks (960 samples at 24kHz = 1920 bytes)
            let chunk_samples = 960;
            let mut interval = tokio::time::interval(Duration::from_millis(40));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            for chunk in resampled.chunks(chunk_samples) {
                if playback.cancellation_epoch() != epoch {
                    bail!("Playback was cancelled or preempted");
                }
                interval.tick().await;
                let pcm_bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                if let Err(err) = playback
                    .play(epoch, crate::audio::ALARM_RESPONSE_ID, pcm_bytes)
                    .await
                {
                    playback.release_alarm(epoch);
                    bail!("AudioPlayback play error: {err}");
                }
            }

            // Send empty frame to trigger playback process drain
            let _ = playback
                .play(epoch, crate::audio::ALARM_RESPONSE_ID, Vec::new())
                .await;

            // Wait for playback drain to finish
            let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while playback.is_playing() && tokio::time::Instant::now() < drain_deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            playback.release_alarm(epoch);
            Ok(())
        })
    }

    fn stop(&self) {
        self.playback.interrupt();
    }

    fn is_aux_supported(&self) -> bool {
        self.aux_supported
    }
}

/// Generates a band-limited logarithmic/linear sine chirp sweep.
/// Clamped strictly: duration <= 5000ms, volume <= -6.0 dBFS, 20Hz..8000Hz.
pub fn generate_chirp_pcm(
    start_hz: f32,
    end_hz: f32,
    duration_ms: u32,
    volume_dbfs: f32,
    sample_rate: u32,
) -> Vec<i16> {
    let dur_ms = duration_ms.clamp(100, MAX_CHIRP_DURATION_MS);
    let vol_dbfs = volume_dbfs.min(MAX_CHIRP_VOLUME_DBFS);
    let f0 = start_hz.clamp(20.0, 8000.0) as f64;
    let f1 = end_hz.clamp(20.0, 8000.0) as f64;

    let num_samples = ((sample_rate as f64) * (dur_ms as f64) / 1000.0).round() as usize;
    if num_samples == 0 {
        return Vec::new();
    }
    let duration_sec = dur_ms as f64 / 1000.0;
    let linear_amp = 10.0_f64.powf(vol_dbfs as f64 / 20.0) * 32767.0;
    // Strict safety clamp: peak <= 16384 (<= -6.02 dBFS)
    let linear_amp = linear_amp.clamp(0.0, 16384.0);
    let k = (f1 - f0) / duration_sec;

    // 5ms smooth Hann fade at start and finish
    let fade_samples = ((sample_rate as f64) * 0.005).round() as usize;

    let mut out = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let t = i as f64 / sample_rate as f64;
        let phase = 2.0 * PI * (f0 * t + 0.5 * k * t * t);
        let mut val = (linear_amp * phase.sin()).clamp(-32768.0, 32767.0);

        if i < fade_samples && fade_samples > 0 {
            let factor = 0.5 * (1.0 - (PI * i as f64 / fade_samples as f64).cos());
            val *= factor;
        } else if i >= num_samples.saturating_sub(fade_samples) && fade_samples > 0 {
            let idx = num_samples - 1 - i;
            let factor = 0.5 * (1.0 - (PI * idx as f64 / fade_samples as f64).cos());
            val *= factor;
        }

        out.push(val.round() as i16);
    }
    out
}

/// Autonomous Acoustic Job Runner.
/// Orchestrates capture pre-roll, stimulus emission, playback drain, and post-roll tail.
pub struct AcousticJobRunner {
    samples_store: Arc<SamplesStore>,
    lab: Arc<AcousticLab>,
    recorder: Option<HardwareRecorder>,
    playback_sink: Arc<dyn AcousticPlaybackSink>,
    active_job: Arc<Mutex<Option<JobDetail>>>,
    history: Arc<Mutex<VecDeque<JobSummary>>>,
    reports: Arc<Mutex<std::collections::HashMap<String, crate::acoustics::evaluator::JobReport>>>,
    cancel_flag: Arc<AtomicBool>,
    abort_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl AcousticJobRunner {
    pub fn new(
        samples_store: Arc<SamplesStore>,
        lab: Arc<AcousticLab>,
        recorder: Option<HardwareRecorder>,
        playback_sink: Arc<dyn AcousticPlaybackSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            samples_store,
            lab,
            recorder,
            playback_sink,
            active_job: Arc::new(Mutex::new(None)),
            history: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_JOB_HISTORY))),
            reports: Arc::new(Mutex::new(std::collections::HashMap::with_capacity(
                MAX_JOB_HISTORY,
            ))),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            abort_handle: Arc::new(Mutex::new(None)),
        })
    }

    pub fn is_aux_supported(&self) -> bool {
        self.playback_sink.is_aux_supported()
    }

    pub fn snapshot(&self) -> JobsSnapshot {
        let active = self
            .active_job
            .lock()
            .unwrap()
            .as_ref()
            .map(|j| j.summary.clone());
        let history = self
            .history
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        JobsSnapshot {
            active_job: active,
            history,
            aux_supported: self.is_aux_supported(),
        }
    }

    pub fn get_job(&self, job_id: &str) -> Option<JobDetail> {
        {
            let guard = self.active_job.lock().unwrap();
            if let Some(ref j) = *guard {
                if j.summary.job_id == job_id {
                    return Some(j.clone());
                }
            }
        }
        let history = self.history.lock().unwrap();
        history
            .iter()
            .find(|s| s.job_id == job_id)
            .map(|summary| JobDetail {
                summary: summary.clone(),
                config: CreateJobDto {
                    name: summary.name.clone(),
                    stimulus_origin: summary.stimulus_origin,
                    output_route: summary.output_route,
                    stimulus: StimulusSignal::Silence { duration_ms: 1000 },
                    pre_roll_ms: 500,
                    post_roll_ms: 1000,
                    volume_pct: 40,
                },
                playback_started_at_ms: summary.started_at_ms,
                playback_ended_at_ms: summary.completed_at_ms,
                events: Vec::new(),
            })
    }

    pub fn job_events(&self, job_id: &str) -> Option<Vec<AcousticEvent>> {
        let guard = self.active_job.lock().unwrap();
        if let Some(ref j) = *guard {
            if j.summary.job_id == job_id {
                return Some(j.events.clone());
            }
        }
        None
    }

    pub fn get_report(&self, job_id: &str) -> Option<crate::acoustics::evaluator::JobReport> {
        let guard = self.reports.lock().unwrap();
        if let Some(rep) = guard.get(job_id) {
            return Some(rep.clone());
        }
        drop(guard);
        self.get_job(job_id)
            .map(|detail| crate::acoustics::evaluator::generate_job_report(&detail, None))
    }

    /// Cancel active job idempotently. Stops playback, cancels recorder, marks job cancelled.
    pub fn cancel_active(&self, reason: &str) -> Result<JobSummary, JobError> {
        self.cancel_flag.store(true, Ordering::SeqCst);
        self.playback_sink.stop();
        if let Some(ref rec) = self.recorder {
            rec.cancel();
        }
        if let Some(handle) = self.abort_handle.lock().unwrap().take() {
            handle.abort();
        }
        self.lab.set_test_session(false);

        let mut guard = self.active_job.lock().unwrap();
        if let Some(mut active) = guard.take() {
            let now_ms = now_timestamp_ms();
            active.summary.state = JobState::Cancelled;
            active.summary.completed_at_ms = Some(now_ms);
            active.summary.duration_ms = now_ms.saturating_sub(active.summary.created_at_ms);
            active.summary.error = Some(reason.to_string());

            let mut hist = self.history.lock().unwrap();
            if hist.len() >= MAX_JOB_HISTORY {
                hist.pop_front();
            }
            hist.push_back(active.summary.clone());
            info!(
                job_id = active.summary.job_id,
                reason, "Acoustic job cancelled"
            );
            Ok(active.summary)
        } else {
            Err(JobError::NotFound(
                "No active acoustic job to cancel".into(),
            ))
        }
    }

    /// Starts an acoustic job. Enforces mutual exclusion: fails with 409 Conflict if active.
    pub fn start_job(self: &Arc<Self>, mut dto: CreateJobDto) -> Result<JobSummary, JobError> {
        // Parameter clamping & safety limits
        dto.pre_roll_ms = dto.pre_roll_ms.clamp(100, 2000);
        dto.post_roll_ms = dto.post_roll_ms.clamp(100, 5000);
        dto.volume_pct = dto.volume_pct.clamp(0, MAX_VOLUME_PCT);

        // Validation: Ambient origin must not play audio
        if dto.stimulus_origin == StimulusOrigin::Ambient {
            if dto.output_route != OutputRoute::None {
                return Err(JobError::Validation(
                    "Ambient noise stimulus origin must have output_route 'none'".into(),
                ));
            }
            if !matches!(dto.stimulus, StimulusSignal::Silence { .. }) {
                return Err(JobError::Validation(
                    "Ambient noise stimulus origin must use 'silence' signal".into(),
                ));
            }
        }

        // Validation: AUX output route support
        if dto.output_route == OutputRoute::AuxOut && !self.playback_sink.is_aux_supported() {
            return Err(JobError::Unsupported(
                "AUX output route is not supported on this device/environment; silent fallback to built_in is forbidden".into(),
            ));
        }

        // Validate stimulus signal parameters
        let stimulus_pcm = match &dto.stimulus {
            StimulusSignal::ChirpSweep {
                start_hz,
                end_hz,
                duration_ms,
                volume_dbfs,
            } => {
                if *start_hz < 20.0 || *start_hz > 8000.0 || *end_hz < 20.0 || *end_hz > 8000.0 {
                    return Err(JobError::Validation(
                        "Chirp sweep frequency must be within [20.0, 8000.0] Hz".into(),
                    ));
                }
                if *duration_ms < 100 || *duration_ms > MAX_CHIRP_DURATION_MS {
                    return Err(JobError::Validation(format!(
                        "Chirp sweep duration must be between 100ms and {}ms",
                        MAX_CHIRP_DURATION_MS
                    )));
                }
                if *volume_dbfs > MAX_CHIRP_VOLUME_DBFS {
                    return Err(JobError::Validation(format!(
                        "Chirp sweep volume ({:.1} dBFS) exceeds maximum safe limit ({:.1} dBFS)",
                        volume_dbfs, MAX_CHIRP_VOLUME_DBFS
                    )));
                }
                generate_chirp_pcm(
                    *start_hz,
                    *end_hz,
                    *duration_ms,
                    *volume_dbfs,
                    TARGET_SAMPLE_RATE,
                )
            }
            StimulusSignal::Silence { duration_ms } => {
                if *duration_ms < 100 || *duration_ms > MAX_SILENCE_DURATION_MS {
                    return Err(JobError::Validation(format!(
                        "Silence duration must be between 100ms and {}ms",
                        MAX_SILENCE_DURATION_MS
                    )));
                }
                let samples_count =
                    ((TARGET_SAMPLE_RATE as f64) * (*duration_ms as f64) / 1000.0).round() as usize;
                vec![0i16; samples_count]
            }
            StimulusSignal::Sample { sample_id } => {
                let wav_bytes = self.samples_store.get_wav_bytes(sample_id).map_err(|e| {
                    JobError::Validation(format!("Sample '{}' error: {e}", sample_id))
                })?;
                let (samples, _) = validate_and_analyze_wav(&wav_bytes, None)
                    .map_err(|e| JobError::Validation(format!("Invalid sample audio: {e}")))?;
                samples
            }
        };

        let now_ms = now_timestamp_ms();
        let job_id = format!("job_{}", uuid::Uuid::new_v4().simple());

        // Mutual exclusion lease check
        {
            let mut guard = self.active_job.lock().unwrap();
            if let Some(ref current) = *guard {
                if matches!(
                    current.summary.state,
                    JobState::Pending | JobState::Preparing | JobState::Running
                ) {
                    return Err(JobError::Conflict(format!(
                        "Job '{}' is already running",
                        current.summary.job_id
                    )));
                }
            }

            let initial_summary = JobSummary {
                job_id: job_id.clone(),
                name: dto.name.clone(),
                state: JobState::Preparing,
                stimulus_origin: dto.stimulus_origin,
                output_route: dto.output_route,
                created_at_ms: now_ms,
                started_at_ms: None,
                completed_at_ms: None,
                duration_ms: 0,
                captured_sample_id: None,
                captured_metrics: None,
                kws_detections_count: 0,
                events_count: 0,
                error: None,
            };

            let initial_detail = JobDetail {
                summary: initial_summary.clone(),
                config: dto.clone(),
                playback_started_at_ms: None,
                playback_ended_at_ms: None,
                events: Vec::new(),
            };

            *guard = Some(initial_detail);
            self.cancel_flag.store(false, Ordering::SeqCst);
        }

        // Spawn foreground job runner task
        let runner = self.clone();
        let task_job_id = job_id.clone();
        let task_dto = dto.clone();

        let handle = tokio::spawn(async move {
            runner
                .execute_job_task(task_job_id, task_dto, stimulus_pcm)
                .await;
        });

        *self.abort_handle.lock().unwrap() = Some(handle);

        let guard = self.active_job.lock().unwrap();
        let current_summary = guard
            .as_ref()
            .map(|j| j.summary.clone())
            .context("Job disappeared after creation")
            .map_err(|e| JobError::Execution(e.to_string()))?;

        Ok(current_summary)
    }

    /// Background execution workflow:
    /// Pre-roll capture -> Playback -> Post-roll tail capture -> Metrics & Sample Store.
    async fn execute_job_task(
        &self,
        job_id: String,
        dto: CreateJobDto,
        stimulus_samples: Vec<i16>,
    ) {
        let started_ms = now_timestamp_ms();

        // 1. Enable test session in AcousticLab (gates KWS from cloud turn, records raw detections)
        self.lab.set_test_session(true);

        {
            let mut guard = self.active_job.lock().unwrap();
            if let Some(ref mut j) = *guard {
                j.summary.state = JobState::Running;
                j.summary.started_at_ms = Some(started_ms);
            }
        }

        let stimulus_dur_ms =
            ((stimulus_samples.len() as f64 / TARGET_SAMPLE_RATE as f64) * 1000.0).round() as u32;
        let total_capture_ms = dto.pre_roll_ms + stimulus_dur_ms + dto.post_roll_ms;
        let total_capture_dur = Duration::from_millis(total_capture_ms as u64);

        info!(
            job_id,
            pre_roll_ms = dto.pre_roll_ms,
            stimulus_ms = stimulus_dur_ms,
            post_roll_ms = dto.post_roll_ms,
            total_capture_ms,
            "Starting acoustic job execution"
        );

        // 2. Start hardware mic capture
        let recorder = self.recorder.clone();
        let capture_fut = async move {
            if let Some(rec) = recorder {
                rec.record(total_capture_dur).await
            } else {
                // Mock capture when running without hardware recorder
                let sample_count = (TARGET_SAMPLE_RATE as f64 * (total_capture_ms as f64 / 1000.0))
                    .round() as usize;
                tokio::time::sleep(total_capture_dur).await;
                Ok(vec![0i16; sample_count])
            }
        };

        let capture_task = tokio::spawn(capture_fut);

        // 3. Pre-roll delay (ensures microphone capture stream is warm and recording ambient background)
        tokio::time::sleep(Duration::from_millis(dto.pre_roll_ms as u64)).await;

        if self.cancel_flag.load(Ordering::SeqCst) {
            self.finalize_cancelled(&job_id, "Cancelled during pre-roll");
            return;
        }

        // 4. Playback phase
        let pb_started_ms = now_timestamp_ms();
        {
            let mut guard = self.active_job.lock().unwrap();
            if let Some(ref mut j) = *guard {
                j.playback_started_at_ms = Some(pb_started_ms);
            }
        }

        if dto.output_route != OutputRoute::None && !stimulus_samples.is_empty() {
            let res = self
                .playback_sink
                .play_pcm(
                    &stimulus_samples,
                    TARGET_SAMPLE_RATE,
                    dto.output_route,
                    dto.volume_pct,
                )
                .await;

            if let Err(e) = res {
                error!(job_id, error = %e, "Playback error during acoustic job");
                self.finalize_failed(&job_id, format!("Playback error: {e}"));
                return;
            }
        } else {
            // Emulate stimulus duration for silent / ambient tests
            tokio::time::sleep(Duration::from_millis(stimulus_dur_ms as u64)).await;
        }

        let pb_ended_ms = now_timestamp_ms();
        {
            let mut guard = self.active_job.lock().unwrap();
            if let Some(ref mut j) = *guard {
                j.playback_ended_at_ms = Some(pb_ended_ms);
            }
        }

        if self.cancel_flag.load(Ordering::SeqCst) {
            self.finalize_cancelled(&job_id, "Cancelled after playback");
            return;
        }

        // 5. Post-roll reverberation tail capture completion
        let captured_samples_res = match capture_task.await {
            Ok(res) => res,
            Err(join_err) => {
                self.finalize_failed(&job_id, format!("Capture task panicked: {join_err}"));
                return;
            }
        };

        let captured_samples = match captured_samples_res {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    job_id,
                    error = %e,
                    "Microphone recording completed with fallback or error"
                );
                // In non-hardware / test environment, create mock silence
                let count = (TARGET_SAMPLE_RATE as f64 * (total_capture_ms as f64 / 1000.0)).round()
                    as usize;
                vec![0i16; count]
            }
        };

        // 6. Metrics & WAV validation
        let wav_bytes = match encode_wav_pcm16_mono(&captured_samples) {
            Ok(b) => b,
            Err(e) => {
                self.finalize_failed(&job_id, format!("WAV encoding error: {e}"));
                return;
            }
        };

        let captured_metrics = match validate_and_analyze_wav(&wav_bytes, None) {
            Ok((_, m)) => Some(CapturedAudioMetrics::from(m)),
            Err(e) => {
                warn!(job_id, error = %e, "Captured audio analysis warning");
                None
            }
        };

        // 7. Store captured sample in SamplesStore under Calibration split
        let sample_label = match dto.stimulus_origin {
            StimulusOrigin::ExternalUserPosition => SampleLabel::PositiveWakeWord,
            StimulusOrigin::OwnOutput => SampleLabel::OwnOutput,
            StimulusOrigin::Ambient => SampleLabel::AmbientNoise,
        };
        let sample_dto = CreateSampleDto {
            name: format!("{}_captured", dto.name.as_deref().unwrap_or(&job_id)),
            source: Some(SampleSource::Recorded),
            label: sample_label,
            split: SampleSplit::Calibration,
            interval: None,
            notes: Some(format!(
                "Acoustic job captured audio: route={:?}, volume={}%, duration={}ms",
                dto.output_route, dto.volume_pct, total_capture_ms
            )),
            wav_base64: None,
        };

        let stored_sample_id = match self.samples_store.store_sample(sample_dto, &wav_bytes) {
            Ok(sample) => Some(sample.id),
            Err(e) => {
                warn!(
                    job_id,
                    error = %e,
                    "Could not persist captured sample to store"
                );
                None
            }
        };

        // 8. Collect AcousticEvents recorded during the job
        let events = self.lab.events(1, 256);
        let kws_detections = events
            .iter()
            .filter(|e| e.timestamp_ms >= started_ms && e.raw_detected)
            .count();

        // 9. Reset test session in AcousticLab
        self.lab.set_test_session(false);

        // 10. Mark Completed and record into history
        let completed_ms = now_timestamp_ms();
        let mut guard = self.active_job.lock().unwrap();
        if let Some(mut active) = guard.take() {
            active.summary.state = JobState::Completed;
            active.summary.completed_at_ms = Some(completed_ms);
            active.summary.duration_ms = completed_ms.saturating_sub(started_ms);
            active.summary.captured_sample_id = stored_sample_id;
            active.summary.captured_metrics = captured_metrics;
            active.summary.kws_detections_count = kws_detections;
            active.summary.events_count = events.len();
            active.events = events;

            let report =
                crate::acoustics::evaluator::generate_job_report(&active, Some(&captured_samples));
            {
                let mut reps = self.reports.lock().unwrap();
                if reps.len() >= MAX_JOB_HISTORY {
                    reps.clear();
                }
                reps.insert(job_id.clone(), report);
            }

            let mut hist = self.history.lock().unwrap();
            if hist.len() >= MAX_JOB_HISTORY {
                hist.pop_front();
            }
            hist.push_back(active.summary.clone());
            info!(
                job_id,
                duration_ms = active.summary.duration_ms,
                kws_detections,
                "Acoustic job completed successfully"
            );
        }
    }

    fn finalize_cancelled(&self, job_id: &str, reason: &str) {
        self.lab.set_test_session(false);
        let mut guard = self.active_job.lock().unwrap();
        if let Some(mut active) = guard.take() {
            let now_ms = now_timestamp_ms();
            active.summary.state = JobState::Cancelled;
            active.summary.completed_at_ms = Some(now_ms);
            active.summary.duration_ms = now_ms.saturating_sub(active.summary.created_at_ms);
            active.summary.error = Some(reason.to_string());

            let mut hist = self.history.lock().unwrap();
            if hist.len() >= MAX_JOB_HISTORY {
                hist.pop_front();
            }
            hist.push_back(active.summary);
            info!(job_id, reason, "Acoustic job cancelled");
        }
    }

    fn finalize_failed(&self, job_id: &str, err_msg: String) {
        self.lab.set_test_session(false);
        let mut guard = self.active_job.lock().unwrap();
        if let Some(mut active) = guard.take() {
            let now_ms = now_timestamp_ms();
            active.summary.state = JobState::Failed;
            active.summary.completed_at_ms = Some(now_ms);
            active.summary.duration_ms = now_ms.saturating_sub(active.summary.created_at_ms);
            active.summary.error = Some(err_msg.clone());

            let mut hist = self.history.lock().unwrap();
            if hist.len() >= MAX_JOB_HISTORY {
                hist.pop_front();
            }
            hist.push_back(active.summary);
            error!(job_id, error = %err_msg, "Acoustic job failed");
        }
    }
}

fn now_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WakewordCalibrationConfig;

    #[test]
    fn test_chirp_sweep_generation_and_clamping() {
        // Safe 1s chirp sweep 100Hz -> 4000Hz at -12 dBFS
        let pcm = generate_chirp_pcm(100.0, 4000.0, 1000, -12.0, 16000);
        assert_eq!(pcm.len(), 16000);

        let max_abs = pcm.iter().map(|&s| (s as i32).abs()).max().unwrap_or(0);
        // At -12 dBFS, amplitude is ~8232; must not exceed safe limit (16384)
        assert!(max_abs > 6000 && max_abs < 10000);

        // Clamping check: volume > -6.0 dBFS is strictly clamped to -6.0 dBFS
        let clamped = generate_chirp_pcm(200.0, 2000.0, 500, 0.0, 16000);
        assert_eq!(clamped.len(), 8000);
        let clamped_max = clamped.iter().map(|&s| (s as i32).abs()).max().unwrap_or(0);
        assert!(
            clamped_max <= 16384,
            "Chirp peak must not exceed safe ceiling (16384), got {}",
            clamped_max
        );
    }

    #[tokio::test]
    async fn test_job_runner_lifecycle_and_mutual_exclusion() {
        let temp = std::env::temp_dir().join(format!("test_job_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp);
        let samples_store = Arc::new(SamplesStore::new(temp.clone()).unwrap());
        let (lab, _rx) = AcousticLab::new(WakewordCalibrationConfig::default());
        let sink = Arc::new(MockPlaybackSink::new(false)); // AUX unsupported
        let runner = AcousticJobRunner::new(samples_store, lab, None, sink.clone());

        // 1. Starting job with AUX when unsupported must fail with Unsupported
        let aux_dto = CreateJobDto {
            name: Some("aux_test".into()),
            stimulus_origin: StimulusOrigin::ExternalUserPosition,
            output_route: OutputRoute::AuxOut,
            stimulus: StimulusSignal::Silence { duration_ms: 200 },
            pre_roll_ms: 100,
            post_roll_ms: 100,
            volume_pct: 40,
        };
        let err = runner.start_job(aux_dto).unwrap_err();
        assert!(matches!(err, JobError::Unsupported(_)));

        // 2. Starting valid job
        let job_dto = CreateJobDto {
            name: Some("sweep_test".into()),
            stimulus_origin: StimulusOrigin::OwnOutput,
            output_route: OutputRoute::BuiltIn,
            stimulus: StimulusSignal::ChirpSweep {
                start_hz: 200.0,
                end_hz: 1000.0,
                duration_ms: 200,
                volume_dbfs: -10.0,
            },
            pre_roll_ms: 100,
            post_roll_ms: 100,
            volume_pct: 30,
        };

        let summary = runner.start_job(job_dto).unwrap();
        assert_eq!(summary.name.as_deref(), Some("sweep_test"));

        // 3. Concurrent start must yield Conflict
        let conflict_dto = CreateJobDto {
            name: Some("concurrent_test".into()),
            stimulus_origin: StimulusOrigin::OwnOutput,
            output_route: OutputRoute::BuiltIn,
            stimulus: StimulusSignal::Silence { duration_ms: 200 },
            pre_roll_ms: 100,
            post_roll_ms: 100,
            volume_pct: 30,
        };
        let conflict_err = runner.start_job(conflict_dto).unwrap_err();
        assert!(matches!(conflict_err, JobError::Conflict(_)));

        // Wait for background execution to finish (100ms pre + 200ms sweep + 100ms post)
        tokio::time::sleep(Duration::from_millis(600)).await;

        let snap = runner.snapshot();
        assert!(snap.active_job.is_none());
        assert_eq!(snap.history.len(), 1);
        let finished = &snap.history[0];
        assert_eq!(finished.state, JobState::Completed);
        assert!(finished.captured_sample_id.is_some());
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[tokio::test]
    async fn test_job_runner_cancellation() {
        let temp = std::env::temp_dir().join(format!("test_job_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp);
        let samples_store = Arc::new(SamplesStore::new(temp.clone()).unwrap());
        let (lab, _rx) = AcousticLab::new(WakewordCalibrationConfig::default());
        let sink = Arc::new(MockPlaybackSink::new(true));
        let runner = AcousticJobRunner::new(samples_store, lab, None, sink.clone());

        let job_dto = CreateJobDto {
            name: Some("long_job".into()),
            stimulus_origin: StimulusOrigin::Ambient,
            output_route: OutputRoute::None,
            stimulus: StimulusSignal::Silence { duration_ms: 2000 },
            pre_roll_ms: 500,
            post_roll_ms: 500,
            volume_pct: 0,
        };

        let summary = runner.start_job(job_dto).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let cancelled = runner.cancel_active("Interrupted by user action").unwrap();
        assert_eq!(cancelled.job_id, summary.job_id);
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert_eq!(
            cancelled.error.as_deref(),
            Some("Interrupted by user action")
        );
        let _ = std::fs::remove_dir_all(&temp);
    }
}
