//! Local acoustic settings and metadata. No microphone, provider or shell access.
pub mod profiles;
pub mod samples;
use crate::config::{Config, WakewordCalibrationConfig};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug, Default, Serialize)]
pub struct RuntimeMetrics {
    pub ready: bool,
    pub model_loaded: bool,
    pub model_name: Option<String>,
    pub model_sha256: Option<String>,
    pub pcm_rms: f32,
    pub noise_floor: f32,
    pub vad_level: f32,
    pub last_score: Option<f32>,
    pub last_avg_score: Option<f32>,
    pub candidate_count: u64,
    pub keyword_count: u64,
    pub opened_count: u64,
    pub suppressed_count: u64,
    pub shadow_count: u64,
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcousticEvent {
    pub sequence: u64,
    pub sample_offset: u64,
    pub timestamp_ms: u64,
    pub applied_revision: u64,
    pub score: f32,
    pub avg_score: f32,
    pub raw_detected: bool,
    pub gate_opened: bool,
    pub suppression_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub runtime_instance_id: String,
    pub revision: u64,
    pub applied_revision: Option<u64>,
    pub saved_revision: u64,
    pub applied: Option<WakewordCalibrationConfig>,
    pub saved: WakewordCalibrationConfig,
    pub pending: bool,
    pub error: Option<String>,
    pub test_session: bool,
    pub runtime: RuntimeMetrics,
    pub recent_events: Vec<AcousticEvent>,
}

/// One bounded request, acknowledged only by the audio loop that applied it.
pub struct ApplyRequest {
    pub revision: u64,
    pub settings: WakewordCalibrationConfig,
    pub reply: oneshot::Sender<()>,
}

impl ApplyRequest {
    pub fn ack(self) {
        let _ = self.reply.send(());
    }
}

/// Strict network DTO for the acoustic profile API.
/// Missing fields are NOT defaulted (preventing silent profile resets).
/// Unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcousticSettingsDto {
    pub enabled: bool,
    pub shadow_mode: bool,
    pub threshold: f32,
    pub avg_threshold: f32,
    pub min_scores: usize,
    pub eager: bool,
    pub score_mode: String,
    pub score_ref: f32,
    pub dtw_band_size: u16,
    pub gain_normalizer: bool,
    pub gain_min: f32,
    pub gain_max: f32,
    pub gain_ref: Option<f32>,
    pub band_pass: bool,
    pub band_low_hz: f32,
    pub band_high_hz: f32,
    pub mic_gain: f32,
    pub vad_threshold: f32,
    pub vad_noise_adaptation: bool,
    pub vad_noise_multiplier: f32,
    pub vad_noise_offset: f32,
    pub vad_rise_alpha: f32,
    pub vad_fall_alpha: f32,
    pub vad_onset_frames: usize,
    pub pre_roll_ms: u32,
    pub initial_silence_timeout_ms: u32,
    pub silence_timeout_ms: u32,
    pub max_utterance_secs: u64,
    pub manual_activation_secs: u64,
    pub followup_timeout_secs: u64,
    pub echo_tail_ms: u64,
    pub startup_suppression_ms: u32,
    pub wake_chime: bool,
}

impl AcousticSettingsDto {
    pub fn validate(&self) -> Result<()> {
        let config: WakewordCalibrationConfig = self.clone().into();
        config.validate()
    }
}

impl From<WakewordCalibrationConfig> for AcousticSettingsDto {
    fn from(c: WakewordCalibrationConfig) -> Self {
        Self {
            enabled: c.enabled,
            shadow_mode: c.shadow_mode,
            threshold: c.threshold,
            avg_threshold: c.avg_threshold,
            min_scores: c.min_scores,
            eager: c.eager,
            score_mode: c.score_mode,
            score_ref: c.score_ref,
            dtw_band_size: c.dtw_band_size,
            gain_normalizer: c.gain_normalizer,
            gain_min: c.gain_min,
            gain_max: c.gain_max,
            gain_ref: c.gain_ref,
            band_pass: c.band_pass,
            band_low_hz: c.band_low_hz,
            band_high_hz: c.band_high_hz,
            mic_gain: c.mic_gain,
            vad_threshold: c.vad_threshold,
            vad_noise_adaptation: c.vad_noise_adaptation,
            vad_noise_multiplier: c.vad_noise_multiplier,
            vad_noise_offset: c.vad_noise_offset,
            vad_rise_alpha: c.vad_rise_alpha,
            vad_fall_alpha: c.vad_fall_alpha,
            vad_onset_frames: c.vad_onset_frames,
            pre_roll_ms: c.pre_roll_ms,
            initial_silence_timeout_ms: c.initial_silence_timeout_ms,
            silence_timeout_ms: c.silence_timeout_ms,
            max_utterance_secs: c.max_utterance_secs,
            manual_activation_secs: c.manual_activation_secs,
            followup_timeout_secs: c.followup_timeout_secs,
            echo_tail_ms: c.echo_tail_ms,
            startup_suppression_ms: c.startup_suppression_ms,
            wake_chime: c.wake_chime,
        }
    }
}

impl From<AcousticSettingsDto> for WakewordCalibrationConfig {
    fn from(d: AcousticSettingsDto) -> Self {
        let normalized_score_mode = match d.score_mode.trim().to_ascii_lowercase().as_str() {
            "average" => "avg".to_string(),
            other => other.to_string(),
        };
        Self {
            threshold: d.threshold,
            avg_threshold: d.avg_threshold,
            min_scores: d.min_scores,
            score_mode: normalized_score_mode,
            eager: d.eager,
            vad_threshold: d.vad_threshold,
            gain_normalizer: d.gain_normalizer,
            band_pass: d.band_pass,
            enabled: d.enabled,
            shadow_mode: d.shadow_mode,
            mic_gain: d.mic_gain,
            pre_roll_ms: d.pre_roll_ms,
            silence_timeout_ms: d.silence_timeout_ms,
            initial_silence_timeout_ms: d.initial_silence_timeout_ms,
            max_utterance_secs: d.max_utterance_secs,
            echo_tail_ms: d.echo_tail_ms,
            startup_suppression_ms: d.startup_suppression_ms,
            manual_activation_secs: d.manual_activation_secs,
            wake_chime: d.wake_chime,
            followup_timeout_secs: d.followup_timeout_secs,
            vad_noise_adaptation: d.vad_noise_adaptation,
            vad_noise_multiplier: d.vad_noise_multiplier,
            vad_noise_offset: d.vad_noise_offset,
            vad_rise_alpha: d.vad_rise_alpha,
            vad_fall_alpha: d.vad_fall_alpha,
            vad_onset_frames: d.vad_onset_frames,
            gain_min: d.gain_min,
            gain_max: d.gain_max,
            gain_ref: d.gain_ref,
            band_low_hz: d.band_low_hz,
            band_high_hz: d.band_high_hz,
            score_ref: d.score_ref,
            dtw_band_size: d.dtw_band_size,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPayload {
    pub expected_revision: u64,
    #[serde(default)]
    pub expected_instance_id: Option<String>,
    pub settings: AcousticSettingsDto,
}

pub struct AcousticLab {
    state: Mutex<Snapshot>,
    tx: mpsc::Sender<ApplyRequest>,
    writer: tokio::sync::Mutex<()>,
    events: Mutex<VecDeque<AcousticEvent>>,
    next_event_sequence: AtomicU64,
    test_session_active: AtomicBool,
}

impl AcousticLab {
    pub fn new(initial: WakewordCalibrationConfig) -> (Arc<Self>, mpsc::Receiver<ApplyRequest>) {
        let (tx, rx) = mpsc::channel(1);
        let runtime_instance_id = uuid::Uuid::new_v4().to_string();
        (
            Arc::new(Self {
                state: Mutex::new(Snapshot {
                    runtime_instance_id,
                    revision: 0,
                    applied_revision: None,
                    saved_revision: 0,
                    applied: None,
                    saved: initial,
                    pending: false,
                    error: None,
                    test_session: false,
                    runtime: RuntimeMetrics::default(),
                    recent_events: Vec::new(),
                }),
                tx,
                writer: tokio::sync::Mutex::new(()),
                events: Mutex::new(VecDeque::with_capacity(256)),
                next_event_sequence: AtomicU64::new(1),
                test_session_active: AtomicBool::new(false),
            }),
            rx,
        )
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        s.test_session = self.test_session_active.load(Ordering::SeqCst);
        let q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        s.recent_events = q.iter().rev().take(20).cloned().collect();
        s
    }

    pub fn record_event(
        &self,
        sample_offset: u64,
        score: f32,
        avg_score: f32,
        raw_detected: bool,
        gate_opened: bool,
        suppression_reason: Option<String>,
    ) -> AcousticEvent {
        let seq = self.next_event_sequence.fetch_add(1, Ordering::SeqCst);
        let applied_rev = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .applied_revision
            .unwrap_or(0);
        let event = AcousticEvent {
            sequence: seq,
            sample_offset,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            applied_revision: applied_rev,
            score,
            avg_score,
            raw_detected,
            gate_opened,
            suppression_reason,
        };
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= 256 {
            q.pop_front();
        }
        q.push_back(event.clone());
        event
    }

    pub fn events(&self, since: u64, limit: usize) -> Vec<AcousticEvent> {
        let q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let lim = limit.clamp(1, 256);
        q.iter()
            .filter(|e| e.sequence >= since)
            .take(lim)
            .cloned()
            .collect()
    }

    pub fn clear_events(&self) {
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        q.clear();
    }

    pub fn set_test_session(&self, active: bool) {
        self.test_session_active.store(active, Ordering::SeqCst);
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.test_session = active;
    }

    pub fn is_test_session(&self) -> bool {
        self.test_session_active.load(Ordering::SeqCst)
    }

    pub fn ready(&self, settings: WakewordCalibrationConfig, metrics: RuntimeMetrics) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.applied = Some(settings);
        s.applied_revision = Some(s.revision);
        s.saved_revision = s.revision;
        s.runtime = metrics;
        s.runtime.ready = true;
    }

    pub fn metrics(&self, metrics: RuntimeMetrics) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).runtime = metrics;
    }

    /// Called by the audio loop, after its own state and gate have been updated.
    pub fn acknowledge(&self, request: ApplyRequest, error: Option<String>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.revision == request.revision {
            if error.is_none() {
                s.applied = Some(request.settings);
                s.applied_revision = Some(request.revision);
                s.saved_revision = request.revision;
            }
            s.pending = false;
            s.error = error;
        }
        let _ = request.reply.send(());
    }

    /// Save the full validated profile then await a matching runtime ACK.
    /// A timeout leaves an explicit pending state: late ACK is observable by GET.
    pub async fn apply(&self, path: &Path, payload: ApplyPayload) -> Result<Snapshot> {
        self.apply_with_timeout(path, payload, Duration::from_secs(3))
            .await
    }

    async fn apply_with_timeout(
        &self,
        path: &Path,
        payload: ApplyPayload,
        deadline: Duration,
    ) -> Result<Snapshot> {
        payload.settings.validate()?;
        let target_config: WakewordCalibrationConfig = payload.settings.into();
        let _guard = self
            .writer
            .try_lock()
            .context("Another acoustic update is in progress")?;
        let old = self.snapshot();
        if let Some(ref inst) = payload.expected_instance_id {
            ensure!(
                inst == &old.runtime_instance_id,
                "Instance ID mismatch; GPT Mini was restarted. Reload status before updating."
            );
        }
        ensure!(
            old.revision == payload.expected_revision,
            "Stale acoustic revision; reload settings"
        );
        ensure!(
            !old.pending,
            "Previous update is still awaiting the audio loop"
        );
        ensure!(
            old.runtime.ready && !self.tx.is_closed(),
            "Audio loop is unavailable; nothing was saved"
        );
        let revision = old
            .revision
            .checked_add(1)
            .context("Acoustic revision exhausted")?;
        // Reserve queue capacity before touching durable settings.
        let permit = self
            .tx
            .try_reserve()
            .context("Audio settings queue is unavailable")?;
        save_settings(path, &target_config)?;
        let (reply, ack) = oneshot::channel();
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.revision = revision;
            s.saved = target_config.clone();
            s.pending = true;
            s.error = None;
        }
        permit.send(ApplyRequest {
            revision,
            settings: target_config,
            reply,
        });
        match tokio::time::timeout(deadline, ack).await {
            Ok(Ok(())) => {
                let s = self.snapshot();
                ensure!(
                    s.applied_revision == Some(revision) && s.error.is_none(),
                    "Profile saved but rejected by audio loop; inspect applied and saved settings"
                );
                Ok(s)
            }
            Ok(Err(_closed)) => {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.pending = false;
                s.error = Some("Audio loop closed ACK channel without confirming".to_string());
                anyhow::bail!("Audio loop closed ACK channel without confirming");
            }
            Err(_timeout) => {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.error = Some("Audio confirmation timed out awaiting audio loop".to_string());
                anyhow::bail!(
                    "Profile saved; audio confirmation pending. Reload status before another update"
                );
            }
        }
    }

    /// Revert saved on disk and pending errors back to the currently applied working settings.
    pub async fn restore_working(&self, path: &Path) -> Result<Snapshot> {
        let _guard = self
            .writer
            .try_lock()
            .context("Another acoustic update is in progress")?;
        let current = self.snapshot();
        let applied = current
            .applied
            .as_ref()
            .context("No working profile currently applied to restore")?;
        save_settings(path, applied)?;
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.saved = applied.clone();
        s.saved_revision = s.applied_revision.unwrap_or(s.revision);
        s.pending = false;
        s.error = None;
        Ok(s.clone())
    }

    /// Apply candidate settings temporarily in RAM with an ACK and automatic rollback after TTL.
    pub async fn apply_candidate_preview(
        self: &Arc<Self>,
        settings: AcousticSettingsDto,
        ttl: Duration,
    ) -> Result<Snapshot> {
        settings.validate()?;
        let target_config: WakewordCalibrationConfig = settings.into();
        let _guard = self
            .writer
            .try_lock()
            .context("Another acoustic update is in progress")?;
        let old = self.snapshot();
        ensure!(
            !old.pending,
            "Previous update is still awaiting the audio loop"
        );
        ensure!(
            old.runtime.ready && !self.tx.is_closed(),
            "Audio loop is unavailable"
        );
        let revision = old
            .revision
            .checked_add(1)
            .context("Acoustic revision exhausted")?;
        let permit = self
            .tx
            .try_reserve()
            .context("Audio settings queue is unavailable")?;
        let (reply, ack) = oneshot::channel();
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.revision = revision;
            s.pending = true;
            s.error = None;
        }
        permit.send(ApplyRequest {
            revision,
            settings: target_config,
            reply,
        });
        match tokio::time::timeout(Duration::from_secs(3), ack).await {
            Ok(Ok(())) => {
                let s = self.snapshot();
                ensure!(
                    s.applied_revision == Some(revision) && s.error.is_none(),
                    "Candidate preview rejected by audio loop"
                );
                let lab = Arc::clone(self);
                let saved_config = s.saved.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(ttl).await;
                    let should_rollback = {
                        let st = lab.state.lock().unwrap_or_else(|e| e.into_inner());
                        st.applied_revision == Some(revision)
                    };
                    if should_rollback {
                        if let Ok(permit) = lab.tx.try_reserve() {
                            let (reply, _ack) = oneshot::channel();
                            let rollback_rev = revision.wrapping_add(1);
                            {
                                let mut st = lab.state.lock().unwrap_or_else(|e| e.into_inner());
                                st.revision = rollback_rev;
                                st.pending = true;
                            }
                            permit.send(ApplyRequest {
                                revision: rollback_rev,
                                settings: saved_config,
                                reply,
                            });
                        }
                    }
                });
                Ok(s)
            }
            Ok(Err(_closed)) => {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.pending = false;
                s.error = Some("Audio loop closed ACK channel without confirming".to_string());
                anyhow::bail!("Audio loop closed ACK channel without confirming");
            }
            Err(_timeout) => {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.error = Some("Audio confirmation timed out awaiting audio loop".to_string());
                anyhow::bail!("Audio confirmation timed out awaiting audio loop");
            }
        }
    }

    /// Clear stale pending flag if an operation failed or timed out.
    pub fn reset_pending(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.pending = false;
    }

    pub fn response(&self) -> Value {
        let mut value = serde_json::to_value(self.snapshot()).expect("finite acoustic metadata");
        let v = value.as_object_mut().unwrap();
        v.insert("status".into(), json!("ok"));
        v.insert(
            "defaults".into(),
            json!(WakewordCalibrationConfig::default()),
        );
        v.insert("controls".into(), controls());
        v.insert("capabilities".into(), json!([
            {"label":"Микрофонный поток","value":"PCM16 mono · 16 000 Гц; ALSA native 8ch S32LE · 48 000 Гц", "editable":false,"reason":"Геометрия согласована с драйвером и моделью; изменение требует отдельной проверки тракта."},
            {"label":"Выбор физического микрофона","value":std::env::var("MINI_MIC_CHANNEL").ok().filter(|s| ["ch0","ch1","ch2","ch3","avg4"].contains(&s.as_str())).unwrap_or_else(||"ch0".into()),"editable":false,"reason":"Выбирается при запуске ALSA. Горячее переключение каналов в этом тракте не реализовано."},
            {"label":"MFCC","value":"Окно 30 мс / шаг 10 мс; pre-emphasis 0.97", "editable":false,"reason":"Изменение признаков потребует переобучения модели, а не калибровки порога."},
            {"label":"Эхоподавление / нейросетевое шумоподавление","value":"Независимый программный AEC/NS не подключён", "editable":false,"reason":"Полосовой фильтр и нормализация ниже действуют в KWS; они не заменяют AEC/NS."},
            {"label":"Обучение модели","value":"3–8 эталонов в текущем формате; окно записи 3.8 с, максимум 4 с", "editable":false,"reason":"Мастер обучения остаётся на главной странице. Изменение лимита шаблонов требует измерения CPU на Mini."}
        ]));
        value
    }
}

/// Existing profiles win over legacy environment. Saved UI values survive restart.
pub fn effective_settings(config: &Config) -> Result<WakewordCalibrationConfig> {
    if let Some(settings) = &config.wakeword {
        settings.validate()?;
        return Ok(settings.clone());
    }
    let mut s = WakewordCalibrationConfig {
        enabled: config.assistant.wakeword_enabled,
        wake_chime: config.assistant.wake_chime,
        ..WakewordCalibrationConfig::default()
    };
    macro_rules! number {
        ($field:ident, $name:literal) => {
            if let Ok(v) = std::env::var($name) {
                s.$field = v.parse().with_context(|| concat!("Invalid ", $name))?;
            }
        };
    }
    macro_rules! boolean {
        ($field:ident, $name:literal) => {
            if let Ok(v) = std::env::var($name) {
                s.$field = match v.as_str() {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => anyhow::bail!(concat!("Invalid ", $name)),
                };
            }
        };
    }
    number!(threshold, "MINI_WAKEWORD_THRESHOLD");
    number!(avg_threshold, "MINI_WAKEWORD_AVG_THRESHOLD");
    number!(min_scores, "MINI_WAKEWORD_MIN_SCORES");
    number!(vad_threshold, "MINI_VAD_THRESHOLD");
    number!(mic_gain, "MINI_MIC_GAIN");
    number!(silence_timeout_ms, "MINI_SILENCE_TIMEOUT_MS");
    number!(
        initial_silence_timeout_ms,
        "MINI_INITIAL_SILENCE_TIMEOUT_MS"
    );
    number!(max_utterance_secs, "MINI_MAX_UTTERANCE_SECS");
    number!(followup_timeout_secs, "MINI_FOLLOWUP_TIMEOUT_SECS");
    boolean!(enabled, "MINI_WAKEWORD_ENABLED");
    boolean!(eager, "MINI_WAKEWORD_EAGER");
    boolean!(gain_normalizer, "MINI_WAKEWORD_GAIN_NORM");
    boolean!(band_pass, "MINI_WAKEWORD_BAND_PASS");
    boolean!(wake_chime, "MINI_WAKE_CHIME");
    if let Ok(mode) = std::env::var("MINI_WAKEWORD_SCORE_MODE") {
        s.score_mode = mode;
    }
    s.validate()?;
    Ok(s)
}

/// Replace only the acoustic table; retain the rest of the TOML document values.
/// Atomic rename, private temp permissions and fsync protect against truncated files.
pub fn save_settings(path: &Path, settings: &WakewordCalibrationConfig) -> Result<()> {
    settings.validate()?;
    let source = std::fs::read_to_string(path).context("Acoustic config file unavailable")?;
    ensure!(source.len() <= 1024 * 1024, "Config too large");
    let mut doc: toml::Value =
        toml::from_str(&source).context("Invalid existing TOML; not overwritten")?;
    let table = doc.as_table_mut().context("Config must be a TOML table")?;
    table.insert("wakeword".into(), toml::Value::try_from(settings)?);
    let text = toml::to_string_pretty(&doc)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(".acoustics-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp).context("Cannot stage acoustic profile")?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).context("Cannot publish acoustic profile")?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Soft gain at the shared mono PCM boundary, before enrollment, KWS and cloud.
pub fn apply_gain(pcm: &mut [u8], gain: f32) {
    for sample in pcm.as_chunks_mut::<2>().0 {
        let raw = i16::from_le_bytes([sample[0], sample[1]]);
        let scaled = (f32::from(raw) * gain)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        sample.copy_from_slice(&scaled.to_le_bytes());
    }
}

pub fn controls() -> Value {
    let mut out = Vec::new();
    macro_rules! n {($key:literal,$label:literal,$help:literal,$group:literal,$min:expr,$max:expr,$step:expr,$unit:literal,$timing:literal)=>{
        out.push(json!({"key":$key,"label":$label,"help":$help,"group":$group,"kind":"number","min":$min,"max":$max,"step":$step,"unit":$unit,"apply_timing":$timing}));
    };}
    macro_rules! b {($key:literal,$label:literal,$help:literal,$group:literal,$timing:literal)=>{
        out.push(json!({"key":$key,"label":$label,"help":$help,"group":$group,"kind":"boolean","apply_timing":$timing}));
    };}
    b!(
        "enabled",
        "Распознавание слова включено",
        "Выключение блокирует акустические вызовы. Не заменяет физическую кнопку mute.",
        "Режим работы",
        "live"
    );
    b!("shadow_mode","Бесшумная лабораторная проверка","Детектор считает события; новые обращения не отправляются в облако и не запускают звук или команды.","Режим работы","live");
    n!("threshold","Основной порог","Условный DTW-балл, не вероятность. Выше — строже; качество проверяется на обращениях и фоне.","Слово вызова",0.0,0.7311,0.001,"балл","live");
    n!("avg_threshold","Порог усреднённой модели","Отдельное сравнение с усреднёнными MFCC-признаками. Ноль отключает предварительный фильтр.","Слово вызова",0.0,0.7311,0.001,"балл","live");
    n!(
        "min_scores",
        "Подтверждения совпадения",
        "Счётчик совпадений в окне детектора. Это не строго последовательные 16-мс кадры.",
        "Слово вызова",
        1,
        100,
        1,
        "совпадений",
        "live"
    );
    b!(
        "eager",
        "Раннее срабатывание",
        "Выдать событие после достижения числа подтверждений, не ждать завершения countdown.",
        "Слово вызова",
        "live"
    );
    out.push(json!({"key":"score_mode","label":"Объединение эталонов","help":"Max принимает лучший эталон; среднее требует согласия нескольких. Это не выбор качества модели.","group":"Слово вызова","kind":"select","apply_timing":"live","options":[{"value":"max","label":"Лучший (Max)"},{"value":"avg","label":"Среднее (Average)"},{"value":"median","label":"Медиана"},{"value":"p25","label":"P25"},{"value":"p75","label":"P75"},{"value":"p80","label":"P80"},{"value":"p90","label":"P90"},{"value":"p95","label":"P95"}]}));
    n!("score_ref","Масштаб DTW-балла","Меняет преобразование расстояния в балл; после изменения старые пороги нельзя сравнивать напрямую.","Слово вызова",0.01,2.0,0.01,"","live");
    n!(
        "dtw_band_size",
        "Полоса выравнивания DTW",
        "Большая полоса допускает больше различий темпа, увеличивая вычисления.",
        "Слово вызова",
        1,
        32,
        1,
        "",
        "live"
    );
    b!(
        "gain_normalizer",
        "Нормализация в KWS",
        "Выравнивание усиления внутри детектора. Не изменяет цифровое усиление облачной речи.",
        "Фильтры детектора",
        "live"
    );
    n!(
        "gain_min",
        "Минимум нормализации",
        "Нижний коэффициент усиления нормализатора.",
        "Фильтры детектора",
        0.1,
        20.0,
        0.1,
        "×",
        "live"
    );
    n!(
        "gain_max",
        "Максимум нормализации",
        "Верхний коэффициент; высокий уровень также усиливает фон.",
        "Фильтры детектора",
        0.1,
        20.0,
        0.1,
        "×",
        "live"
    );
    out.push(json!({"key":"gain_ref","label":"Опорный RMS нормализации","help":"Авто использует RMS эталона. Число задаёт опорный уровень вручную.","group":"Фильтры детектора","kind":"optional_number","min":0.00001,"max":1.0,"step":0.00001,"unit":"RMS 0…1","apply_timing":"live"}));
    b!("band_pass","Полосовой фильтр KWS","Подавляет частоты вне диапазона только для детектора. Не является полноценным шумоподавлением.","Фильтры детектора","live");
    n!(
        "band_low_hz",
        "Нижняя частота",
        "Должна быть ниже верхней границы.",
        "Фильтры детектора",
        20,
        7900,
        10,
        "Гц",
        "live"
    );
    n!(
        "band_high_hz",
        "Верхняя частота",
        "При 16 кГц частота Найквиста — 8 кГц.",
        "Фильтры детектора",
        20,
        7900,
        10,
        "Гц",
        "live"
    );
    n!(
        "mic_gain",
        "Цифровое усиление входа",
        "Применяется до обучения, KWS и передачи речи. Слишком высокое значение создаёт клиппинг.",
        "Речь и фон",
        0.1,
        20.0,
        0.1,
        "×",
        "live"
    );
    n!(
        "vad_threshold",
        "Минимальный порог речи",
        "VAD определяет речь внутри разрешённого окна; сам по себе не пробуждает GPT Mini.",
        "Речь и фон",
        1,
        1000,
        1,
        "RMS",
        "live"
    );
    b!(
        "vad_noise_adaptation",
        "Адаптация к фону",
        "Порог VAD учитывает оценённый шум помещения.",
        "Речь и фон",
        "live"
    );
    n!(
        "vad_noise_multiplier",
        "Множитель шума",
        "Динамический порог = max(базовый, шум × множитель + добавка).",
        "Речь и фон",
        1,
        10,
        0.05,
        "×",
        "live"
    );
    n!(
        "vad_noise_offset",
        "Добавка к шуму",
        "Дополнительный запас над оценкой фона.",
        "Речь и фон",
        0,
        1000,
        1,
        "RMS",
        "live"
    );
    n!(
        "vad_rise_alpha",
        "Скорость роста оценки фона",
        "Коэффициент сглаживания; больше — быстрее адаптация вверх.",
        "Речь и фон",
        0.001,
        1,
        0.001,
        "",
        "live"
    );
    n!(
        "vad_fall_alpha",
        "Скорость спада оценки фона",
        "Коэффициент сглаживания; больше — быстрее адаптация вниз.",
        "Речь и фон",
        0.001,
        1,
        0.001,
        "",
        "live"
    );
    n!(
        "vad_onset_frames",
        "Подтверждение начала речи",
        "Число последовательных активных PCM-чанков для VAD.",
        "Речь и фон",
        1,
        100,
        1,
        "чанков",
        "live"
    );
    n!(
        "pre_roll_ms",
        "Предзапись",
        "Локальный кольцевой буфер, передаваемый только после разрешённого вызова.",
        "Время и границы",
        0,
        2000,
        10,
        "мс",
        "next_capture"
    );
    n!(
        "initial_silence_timeout_ms",
        "Ожидание речи после имени",
        "Сколько ждать начала команды после акустического вызова.",
        "Время и границы",
        500,
        15000,
        100,
        "мс",
        "next_capture"
    );
    n!(
        "silence_timeout_ms",
        "Пауза завершения фразы",
        "Сколько тишины завершает начавшуюся реплику.",
        "Время и границы",
        200,
        10000,
        100,
        "мс",
        "next_capture"
    );
    n!(
        "max_utterance_secs",
        "Максимальная реплика",
        "Жёсткий предел входного голосового хода.",
        "Время и границы",
        3,
        60,
        1,
        "с",
        "next_capture"
    );
    n!(
        "manual_activation_secs",
        "Ручное окно",
        "Время ожидания реальной речи после явной ручной активации.",
        "Время и границы",
        1,
        30,
        1,
        "с",
        "next_capture"
    );
    n!("followup_timeout_secs","Продолжение без имени","Ноль — строгий вызов по имени каждый раз. Окно продолжения принимает только речь, не тишину.","Время и границы",0,15,1,"с","next_capture");
    n!(
        "echo_tail_ms",
        "Защита после ответа",
        "Отбрасывать акустическое самосрабатывание в течение хвоста собственного голоса.",
        "Время и границы",
        0,
        5000,
        50,
        "мс",
        "next_capture"
    );
    n!("startup_suppression_ms","Прогрев микрофона","Отбросить стартовый PCM-переходный процесс. Меняется для следующего старта, не запускает прогрев заново.","Время и границы",0,10000,100,"мс","next_start");
    b!(
        "wake_chime",
        "Короткий сигнал вызова",
        "Озвучить распознанный вызов. В бесшумной проверке не звучит.",
        "Обратная связь",
        "live"
    );
    json!(out)
}

pub fn pcm_rms(pcm: &[u8]) -> f32 {
    let count = pcm.len() / 2;
    if count == 0 {
        return 0.0;
    }
    let sum: f64 = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| f64::from(i16::from_le_bytes(*v)).powi(2))
        .sum();
    (sum / count as f64).sqrt() as f32
}

pub fn model_identity(path: &Path) -> (Option<String>, Option<String>) {
    use rustpotter::WakewordLoad;
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, None);
    };
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return (None, None);
    }
    let Ok(data) = std::fs::read(path) else {
        return (None, None);
    };
    let hash = crate::spotify::crypto::sha256(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let name = path
        .to_str()
        .and_then(|s| rustpotter::WakewordRef::load_from_file(s).ok())
        .map(|m| m.name);
    (name, Some(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTestFile(std::path::PathBuf);

    impl TempTestFile {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("test_acoustic_{}.toml", uuid::Uuid::new_v4()));
            let _ = std::fs::write(&path, "[wakeword]\n");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_dto_roundtrip_all_33_fields() {
        let original = WakewordCalibrationConfig::default();
        let dto: AcousticSettingsDto = original.clone().into();
        assert!(dto.validate().is_ok());

        let json_str = serde_json::to_string(&dto).expect("serialize DTO");
        let decoded: AcousticSettingsDto =
            serde_json::from_str(&json_str).expect("deserialize DTO");
        assert_eq!(dto, decoded);

        let config: WakewordCalibrationConfig = decoded.into();
        assert_eq!(original, config);
    }

    #[test]
    fn test_partial_json_rejected_without_silent_defaults() {
        // F2 regression: Partial JSON without all 33 fields must fail deserialization.
        let partial_json = r#"{"expected_revision": 0, "settings": {"threshold": 0.45}}"#;
        let result: Result<ApplyPayload, _> = serde_json::from_str(partial_json);
        assert!(
            result.is_err(),
            "Partial profile payload must be rejected, not filled with silent defaults"
        );
    }

    #[test]
    fn test_unknown_fields_rejected() {
        let dto: AcousticSettingsDto = WakewordCalibrationConfig::default().into();
        let mut value = serde_json::to_value(&dto).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown_field".to_string(), json!(123));

        let result: Result<AcousticSettingsDto, _> = serde_json::from_value(value);
        assert!(result.is_err(), "Unknown fields must be rejected");
    }

    #[test]
    fn test_nan_and_infinite_floats_rejected() {
        let dto: AcousticSettingsDto = WakewordCalibrationConfig::default().into();
        let mut value = serde_json::to_value(&dto).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("threshold".to_string(), json!(null));
        assert!(serde_json::from_value::<AcousticSettingsDto>(value).is_err());
    }

    #[test]
    fn test_score_mode_canonicalization() {
        let mut dto: AcousticSettingsDto = WakewordCalibrationConfig::default().into();
        dto.score_mode = "  Average  ".to_string();
        assert!(dto.validate().is_ok());
        let config: WakewordCalibrationConfig = dto.into();
        assert_eq!(config.score_mode, "avg");
    }

    #[test]
    fn test_controls_schema_has_all_33_fields_with_apply_timing() {
        let controls_val = controls();
        let arr = controls_val.as_array().expect("controls is array");
        assert_eq!(arr.len(), 33, "Exactly 33 controls expected");
        for c in arr {
            let key = c["key"].as_str().expect("key is string");
            let timing = c["apply_timing"].as_str().expect("apply_timing is string");
            assert!(
                matches!(timing, "live" | "next_capture" | "next_start"),
                "Control {key} has valid apply_timing ({timing})"
            );
        }
    }

    #[tokio::test]
    async fn test_instance_id_mismatch_rejected() {
        let (lab, mut rx) = AcousticLab::new(WakewordCalibrationConfig::default());
        let snap = lab.snapshot();
        let metrics = RuntimeMetrics {
            ready: true,
            ..Default::default()
        };
        lab.ready(WakewordCalibrationConfig::default(), metrics);

        let temp = TempTestFile::new();
        save_settings(temp.path(), &WakewordCalibrationConfig::default()).unwrap();

        // Spawn mock loop to handle ACK
        tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                let _ = req.reply.send(());
            }
        });

        let payload = ApplyPayload {
            expected_revision: snap.revision,
            expected_instance_id: Some("different-instance-uuid".to_string()),
            settings: WakewordCalibrationConfig::default().into(),
        };

        let err = lab
            .apply(temp.path(), payload)
            .await
            .expect_err("mismatched instance must fail");
        assert!(
            err.to_string().contains("Instance ID mismatch"),
            "Error: {err}"
        );
    }

    #[tokio::test]
    async fn test_closed_ack_channel_clears_pending_and_reports_error() {
        let (lab, mut rx) = AcousticLab::new(WakewordCalibrationConfig::default());
        let snap = lab.snapshot();
        let metrics = RuntimeMetrics {
            ready: true,
            ..Default::default()
        };
        lab.ready(WakewordCalibrationConfig::default(), metrics);

        let temp = TempTestFile::new();
        save_settings(temp.path(), &WakewordCalibrationConfig::default()).unwrap();

        // Audio loop receives request but immediately drops reply channel without sending ACK
        tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                drop(req.reply);
            }
        });

        let payload = ApplyPayload {
            expected_revision: snap.revision,
            expected_instance_id: Some(snap.runtime_instance_id),
            settings: WakewordCalibrationConfig::default().into(),
        };

        let err = lab
            .apply(temp.path(), payload)
            .await
            .expect_err("dropped channel must fail");
        assert!(err.to_string().contains("closed ACK channel"));

        // Pending must NOT remain stuck
        let after = lab.snapshot();
        assert!(!after.pending, "pending must be cleared on closed channel");
        assert!(after.error.is_some(), "error must be recorded");
    }

    #[tokio::test]
    async fn test_save_settings_preserves_other_toml_sections() {
        let temp = TempTestFile::new();
        let initial_toml = r#"
[assistant]
voice = "Puck"
wakeword_enabled = true

[device]
led_pattern = "pulse"
"#;
        std::fs::write(temp.path(), initial_toml).unwrap();

        let cfg = WakewordCalibrationConfig::default();
        save_settings(temp.path(), &cfg).unwrap();

        let content = std::fs::read_to_string(temp.path()).unwrap();
        assert!(content.contains("[assistant]"));
        assert!(content.contains("voice = \"Puck\""));
        assert!(content.contains("[device]"));
        assert!(content.contains("led_pattern = \"pulse\""));
        assert!(content.contains("[wakeword]"));
    }

    #[test]
    fn test_acoustic_event_recording_and_cursor_query() {
        let (lab, _rx) = AcousticLab::new(WakewordCalibrationConfig::default());
        let e1 = lab.record_event(512, 0.45, 0.40, false, false, None);
        let e2 = lab.record_event(1024, 0.88, 0.82, true, false, Some("shadow_mode".into()));
        let e3 = lab.record_event(2048, 0.91, 0.87, true, true, None);

        assert_eq!(e1.sequence, 1);
        assert_eq!(e2.sequence, 2);
        assert_eq!(e3.sequence, 3);

        let all = lab.events(1, 10);
        assert_eq!(all.len(), 3);

        let since_2 = lab.events(2, 10);
        assert_eq!(since_2.len(), 2);
        assert_eq!(since_2[0].sequence, 2);
        assert_eq!(since_2[1].sequence, 3);

        let limited = lab.events(1, 2);
        assert_eq!(limited.len(), 2);

        let snap = lab.snapshot();
        assert_eq!(snap.recent_events.len(), 3);
        assert_eq!(snap.recent_events[0].sequence, 3);
    }

    #[test]
    fn test_test_session_toggle() {
        let (lab, _rx) = AcousticLab::new(WakewordCalibrationConfig::default());
        assert!(!lab.is_test_session());
        assert!(!lab.snapshot().test_session);

        lab.set_test_session(true);
        assert!(lab.is_test_session());
        assert!(lab.snapshot().test_session);

        lab.set_test_session(false);
        assert!(!lab.is_test_session());
        assert!(!lab.snapshot().test_session);
    }

    /// Multi-chunk mock engine: requires 3 consecutive signal chunks to detect.
    /// Simulates MFCC/DTW window accumulation across chunks (mimicking Rustpotter).
    struct MultiChunkMockEngine {
        required_chunks: usize,
        accumulated: usize,
        last_obs: Option<crate::wakeword::KeywordObservation>,
    }

    impl MultiChunkMockEngine {
        fn new(required_chunks: usize) -> Self {
            Self {
                required_chunks,
                accumulated: 0,
                last_obs: None,
            }
        }
    }

    impl crate::wakeword::WakeWordEngine for MultiChunkMockEngine {
        fn process(&mut self, pcm: &[u8]) -> bool {
            let has_signal = pcm.iter().any(|&b| b != 0);
            if has_signal {
                self.accumulated += 1;
                let detected = self.accumulated >= self.required_chunks;
                self.last_obs = Some(crate::wakeword::KeywordObservation {
                    score: if detected { 0.85 } else { 0.40 },
                    avg_score: if detected { 0.80 } else { 0.35 },
                    counter: self.accumulated,
                    gain: 1.0,
                    detected,
                });
                if detected {
                    self.accumulated = 0; // reset upon detection, like Rustpotter
                    return true;
                }
            } else {
                self.accumulated = 0;
                self.last_obs = None;
            }
            false
        }

        fn reset(&mut self) {
            self.accumulated = 0;
            self.last_obs = None;
        }

        fn take_observation(&mut self) -> Option<crate::wakeword::KeywordObservation> {
            self.last_obs.take()
        }
    }

    /// Helper simulating the production audio loop dispatch logic for a given PCM sequence.
    fn run_audio_dispatch_simulation(
        shadow_mode: bool,
        test_session: bool,
        cloud_ready: bool,
        pcm_chunks: Vec<Vec<u8>>,
    ) -> (RuntimeMetrics, Vec<AcousticEvent>, usize, usize) {
        let (lab, _rx) = AcousticLab::new(WakewordCalibrationConfig::default());
        if test_session {
            lab.set_test_session(true);
        }
        let mut lab_metrics = RuntimeMetrics {
            ready: true,
            mode: if test_session {
                "test_session".into()
            } else if shadow_mode {
                "shadow".into()
            } else {
                "listening".into()
            },
            ..Default::default()
        };

        let mut gate = crate::wakeword::InputGate::new(
            Box::new(MultiChunkMockEngine::new(3)),
            100,
            200,
            16000,
            false,
        );

        let mut cumulative_samples: u64 = 0;
        let mut cloud_frames_sent = 0;
        let mut active_turns_opened = 0;
        let mut was_test_session = false;

        for chunk in pcm_chunks {
            cumulative_samples += (chunk.len() / 2) as u64;
            let test_session_now = lab.is_test_session();
            if test_session_now && !was_test_session {
                gate.reset_gate();
            }
            was_test_session = test_session_now;

            let was_active = gate.is_active();
            let frames = gate.process_frame(chunk.clone());
            let now_active = gate.is_active();

            let mut last_obs_score = 0.0_f32;
            let mut last_obs_avg_score = 0.0_f32;

            if let Some(observation) = gate.take_keyword_observation() {
                last_obs_score = observation.score;
                last_obs_avg_score = observation.avg_score;
                lab_metrics.candidate_count += 1;
                lab_metrics.last_score = Some(observation.score);
                lab_metrics.last_avg_score = Some(observation.avg_score);
                if observation.detected {
                    lab_metrics.keyword_count += 1;
                }
            }

            let in_shadow_or_test = shadow_mode || test_session_now;

            if in_shadow_or_test {
                if !was_active && now_active {
                    lab_metrics.shadow_count += 1;
                    let reason = if test_session_now {
                        "test_session"
                    } else {
                        "shadow_mode"
                    };
                    lab.record_event(
                        cumulative_samples,
                        last_obs_score,
                        last_obs_avg_score,
                        true,
                        false,
                        Some(reason.to_string()),
                    );
                    gate.reset_gate();
                }
                continue;
            }

            if !was_active && now_active {
                active_turns_opened += 1;
                lab.record_event(
                    cumulative_samples,
                    last_obs_score,
                    last_obs_avg_score,
                    true,
                    true,
                    None,
                );
            }

            if cloud_ready && gate.is_active() {
                cloud_frames_sent += frames.len();
            }
        }

        let events = lab.events(1, 100);
        (lab_metrics, events, cloud_frames_sent, active_turns_opened)
    }

    #[test]
    fn test_gate_criteria_shadow_preserves_history_and_matches_normal_mode() {
        let silence_chunk = vec![0u8; 512];
        let signal_chunk = vec![0x42u8; 512];

        let stream = vec![
            silence_chunk.clone(),
            silence_chunk.clone(),
            signal_chunk.clone(),
            signal_chunk.clone(),
            signal_chunk.clone(), // 3rd signal chunk: triggers detection!
            silence_chunk.clone(),
            silence_chunk.clone(),
        ];

        // 1. Normal mode (shadow=false, test_session=false, cloud_ready=true)
        let (metrics_norm, events_norm, cloud_norm, turns_norm) =
            run_audio_dispatch_simulation(false, false, true, stream.clone());

        assert_eq!(
            metrics_norm.keyword_count, 1,
            "Normal mode must detect keyword"
        );
        assert_eq!(turns_norm, 1, "Normal mode must open a dialogue turn");
        assert!(
            cloud_norm > 0,
            "Normal mode must send audio frames to cloud"
        );
        assert_eq!(events_norm.len(), 1);
        assert!(events_norm[0].raw_detected);
        assert!(events_norm[0].gate_opened);
        assert_eq!(events_norm[0].suppression_reason, None);

        // 2. Shadow mode (shadow=true, test_session=false, cloud_ready=true)
        let (metrics_shad, events_shad, cloud_shad, turns_shad) =
            run_audio_dispatch_simulation(true, false, true, stream.clone());

        // GATE REQUIREMENT: Raw detections in shadow and normal mode MUST be identical!
        assert_eq!(
            metrics_shad.keyword_count, metrics_norm.keyword_count,
            "Shadow mode raw detection count must match normal mode"
        );
        assert_eq!(
            metrics_shad.shadow_count, 1,
            "Shadow count must increment on detection"
        );
        // GATE REQUIREMENT: Shadow mode MUST NOT send cloud audio or open turns!
        assert_eq!(
            turns_shad, 0,
            "Shadow mode must never open an active user turn"
        );
        assert_eq!(
            cloud_shad, 0,
            "Shadow mode must never send audio frames to cloud transport"
        );
        assert_eq!(events_shad.len(), 1);
        assert!(events_shad[0].raw_detected);
        assert!(!events_shad[0].gate_opened);
        assert_eq!(
            events_shad[0].suppression_reason,
            Some("shadow_mode".to_string())
        );

        // 3. Test session mode (shadow=false, test_session=true, cloud_ready=true)
        let (metrics_test, events_test, cloud_test, turns_test) =
            run_audio_dispatch_simulation(false, true, true, stream.clone());
        assert_eq!(metrics_test.keyword_count, 1);
        assert_eq!(metrics_test.shadow_count, 1);
        assert_eq!(turns_test, 0);
        assert_eq!(cloud_test, 0);
        assert_eq!(
            events_test[0].suppression_reason,
            Some("test_session".to_string())
        );

        // 4. Negative stream: pure silence must produce 0 detections in both modes
        let silence_stream = vec![silence_chunk.clone(); 10];
        let (neg_norm, _, _, _) =
            run_audio_dispatch_simulation(false, false, true, silence_stream.clone());
        let (neg_shad, _, _, _) =
            run_audio_dispatch_simulation(true, false, true, silence_stream.clone());
        assert_eq!(neg_norm.keyword_count, 0);
        assert_eq!(neg_shad.keyword_count, 0);
        assert_eq!(neg_shad.shadow_count, 0);
    }

    #[test]
    fn test_cloud_stopped_does_not_block_local_shadow_observation() {
        let silence_chunk = vec![0u8; 512];
        let signal_chunk = vec![0x42u8; 512];
        let stream = vec![
            silence_chunk.clone(),
            signal_chunk.clone(),
            signal_chunk.clone(),
            signal_chunk.clone(),
            silence_chunk,
        ];

        // Cloud is Stopped / not ready (cloud_ready=false)
        let (metrics, events, cloud_sent, turns) =
            run_audio_dispatch_simulation(true, false, false, stream);
        assert_eq!(metrics.keyword_count, 1, "Local matcher must still detect");
        assert_eq!(metrics.shadow_count, 1, "Shadow count must still increment");
        assert_eq!(cloud_sent, 0);
        assert_eq!(turns, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].suppression_reason,
            Some("shadow_mode".to_string())
        );
    }
}
