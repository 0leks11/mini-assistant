//! Local acoustic settings and metadata. No microphone, provider or shell access.
use crate::config::{Config, WakewordCalibrationConfig};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
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

#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub revision: u64,
    pub applied_revision: Option<u64>,
    pub applied: Option<WakewordCalibrationConfig>,
    pub saved: WakewordCalibrationConfig,
    pub pending: bool,
    pub error: Option<String>,
    pub runtime: RuntimeMetrics,
}

/// One bounded request, acknowledged only by the audio loop that applied it.
pub struct ApplyRequest {
    pub revision: u64,
    pub settings: WakewordCalibrationConfig,
    reply: oneshot::Sender<()>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPayload {
    pub expected_revision: u64,
    pub settings: WakewordCalibrationConfig,
}

pub struct AcousticLab {
    state: Mutex<Snapshot>,
    tx: mpsc::Sender<ApplyRequest>,
    writer: tokio::sync::Mutex<()>,
}

impl AcousticLab {
    pub fn new(initial: WakewordCalibrationConfig) -> (Arc<Self>, mpsc::Receiver<ApplyRequest>) {
        let (tx, rx) = mpsc::channel(1);
        (
            Arc::new(Self {
                state: Mutex::new(Snapshot {
                    revision: 0,
                    applied_revision: None,
                    applied: None,
                    saved: initial,
                    pending: false,
                    error: None,
                    runtime: RuntimeMetrics::default(),
                }),
                tx,
                writer: tokio::sync::Mutex::new(()),
            }),
            rx,
        )
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn ready(&self, settings: WakewordCalibrationConfig, metrics: RuntimeMetrics) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.applied = Some(settings);
        s.applied_revision = Some(s.revision);
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
        let _guard = self
            .writer
            .try_lock()
            .context("Another acoustic update is in progress")?;
        let old = self.snapshot();
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
        save_settings(path, &payload.settings)?;
        let (reply, ack) = oneshot::channel();
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.revision = revision;
            s.saved = payload.settings.clone();
            s.pending = true;
            s.error = None;
        }
        permit.send(ApplyRequest {
            revision,
            settings: payload.settings,
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
            _ => anyhow::bail!(
                "Profile saved; audio confirmation pending. Reload status before another update"
            ),
        }
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
    macro_rules! n {($key:literal,$label:literal,$help:literal,$group:literal,$min:expr,$max:expr,$step:expr,$unit:literal)=>{
        out.push(json!({"key":$key,"label":$label,"help":$help,"group":$group,"kind":"number","min":$min,"max":$max,"step":$step,"unit":$unit}));
    };}
    macro_rules! b {($key:literal,$label:literal,$help:literal,$group:literal)=>{
        out.push(json!({"key":$key,"label":$label,"help":$help,"group":$group,"kind":"boolean"}));
    };}
    b!(
        "enabled",
        "Распознавание слова включено",
        "Выключение блокирует акустические вызовы. Не заменяет физическую кнопку mute.",
        "Режим работы"
    );
    b!("shadow_mode","Бесшумная лабораторная проверка","Детектор считает события; новые обращения не отправляются в облако и не запускают звук или команды.","Режим работы");
    n!("threshold","Основной порог","Условный DTW-балл, не вероятность. Выше — строже; качество проверяется на обращениях и фоне.","Слово вызова",0.0,0.7311,0.001,"балл");
    n!("avg_threshold","Порог усреднённой модели","Отдельное сравнение с усреднёнными MFCC-признаками. Ноль отключает предварительный фильтр.","Слово вызова",0.0,0.7311,0.001,"балл");
    n!(
        "min_scores",
        "Подтверждения совпадения",
        "Счётчик совпадений в окне детектора. Это не строго последовательные 16-мс кадры.",
        "Слово вызова",
        1,
        100,
        1,
        "совпадений"
    );
    b!(
        "eager",
        "Раннее срабатывание",
        "Выдать событие после достижения числа подтверждений, не ждать завершения countdown.",
        "Слово вызова"
    );
    out.push(json!({"key":"score_mode","label":"Объединение эталонов","help":"Max принимает лучший эталон; среднее требует согласия нескольких. Это не выбор качества модели.","group":"Слово вызова","kind":"select","options":[{"value":"max","label":"Лучший (Max)"},{"value":"avg","label":"Среднее (Average)"},{"value":"median","label":"Медиана"},{"value":"p25","label":"P25"},{"value":"p75","label":"P75"},{"value":"p80","label":"P80"},{"value":"p90","label":"P90"},{"value":"p95","label":"P95"}]}));
    n!("score_ref","Масштаб DTW-балла","Меняет преобразование расстояния в балл; после изменения старые пороги нельзя сравнивать напрямую.","Слово вызова",0.01,2.0,0.01,"");
    n!(
        "dtw_band_size",
        "Полоса выравнивания DTW",
        "Большая полоса допускает больше различий темпа, увеличивая вычисления.",
        "Слово вызова",
        1,
        32,
        1,
        ""
    );
    b!(
        "gain_normalizer",
        "Нормализация в KWS",
        "Выравнивание усиления внутри детектора. Не изменяет цифровое усиление облачной речи.",
        "Фильтры детектора"
    );
    n!(
        "gain_min",
        "Минимум нормализации",
        "Нижний коэффициент усиления нормализатора.",
        "Фильтры детектора",
        0.1,
        20.0,
        0.1,
        "×"
    );
    n!(
        "gain_max",
        "Максимум нормализации",
        "Верхний коэффициент; высокий уровень также усиливает фон.",
        "Фильтры детектора",
        0.1,
        20.0,
        0.1,
        "×"
    );
    out.push(json!({"key":"gain_ref","label":"Опорный RMS нормализации","help":"Авто использует RMS эталона. Число задаёт опорный уровень вручную.","group":"Фильтры детектора","kind":"optional_number","min":0.00001,"max":1.0,"step":0.00001,"unit":"RMS 0…1"}));
    b!("band_pass","Полосовой фильтр KWS","Подавляет частоты вне диапазона только для детектора. Не является полноценным шумоподавлением.","Фильтры детектора");
    n!(
        "band_low_hz",
        "Нижняя частота",
        "Должна быть ниже верхней границы.",
        "Фильтры детектора",
        20,
        7900,
        10,
        "Гц"
    );
    n!(
        "band_high_hz",
        "Верхняя частота",
        "При 16 кГц частота Найквиста — 8 кГц.",
        "Фильтры детектора",
        20,
        7900,
        10,
        "Гц"
    );
    n!(
        "mic_gain",
        "Цифровое усиление входа",
        "Применяется до обучения, KWS и передачи речи. Слишком высокое значение создаёт клиппинг.",
        "Речь и фон",
        0.1,
        20.0,
        0.1,
        "×"
    );
    n!(
        "vad_threshold",
        "Минимальный порог речи",
        "VAD определяет речь внутри разрешённого окна; сам по себе не пробуждает GPT Mini.",
        "Речь и фон",
        1,
        1000,
        1,
        "RMS"
    );
    b!(
        "vad_noise_adaptation",
        "Адаптация к фону",
        "Порог VAD учитывает оценённый шум помещения.",
        "Речь и фон"
    );
    n!(
        "vad_noise_multiplier",
        "Множитель шума",
        "Динамический порог = max(базовый, шум × множитель + добавка).",
        "Речь и фон",
        1,
        10,
        0.05,
        "×"
    );
    n!(
        "vad_noise_offset",
        "Добавка к шуму",
        "Дополнительный запас над оценкой фона.",
        "Речь и фон",
        0,
        1000,
        1,
        "RMS"
    );
    n!(
        "vad_rise_alpha",
        "Скорость роста оценки фона",
        "Коэффициент сглаживания; больше — быстрее адаптация вверх.",
        "Речь и фон",
        0.001,
        1,
        0.001,
        ""
    );
    n!(
        "vad_fall_alpha",
        "Скорость спада оценки фона",
        "Коэффициент сглаживания; больше — быстрее адаптация вниз.",
        "Речь и фон",
        0.001,
        1,
        0.001,
        ""
    );
    n!(
        "vad_onset_frames",
        "Подтверждение начала речи",
        "Число последовательных активных PCM-чанков для VAD.",
        "Речь и фон",
        1,
        100,
        1,
        "чанков"
    );
    n!(
        "pre_roll_ms",
        "Предзапись",
        "Локальный кольцевой буфер, передаваемый только после разрешённого вызова.",
        "Время и границы",
        0,
        2000,
        10,
        "мс"
    );
    n!(
        "initial_silence_timeout_ms",
        "Ожидание речи после имени",
        "Сколько ждать начала команды после акустического вызова.",
        "Время и границы",
        500,
        15000,
        100,
        "мс"
    );
    n!(
        "silence_timeout_ms",
        "Пауза завершения фразы",
        "Сколько тишины завершает начавшуюся реплику.",
        "Время и границы",
        200,
        10000,
        100,
        "мс"
    );
    n!(
        "max_utterance_secs",
        "Максимальная реплика",
        "Жёсткий предел входного голосового хода.",
        "Время и границы",
        3,
        60,
        1,
        "с"
    );
    n!(
        "manual_activation_secs",
        "Ручное окно",
        "Время ожидания реальной речи после явной ручной активации.",
        "Время и границы",
        1,
        30,
        1,
        "с"
    );
    n!("followup_timeout_secs","Продолжение без имени","Ноль — строгий вызов по имени каждый раз. Окно продолжения принимает только речь, не тишину.","Время и границы",0,15,1,"с");
    n!(
        "echo_tail_ms",
        "Защита после ответа",
        "Отбрасывать акустическое самосрабатывание в течение хвоста собственного голоса.",
        "Время и границы",
        0,
        5000,
        50,
        "мс"
    );
    n!("startup_suppression_ms","Прогрев микрофона","Отбросить стартовый PCM-переходный процесс. Меняется для следующего старта, не запускает прогрев заново.","Время и границы",0,10000,100,"мс");
    b!(
        "wake_chime",
        "Короткий сигнал вызова",
        "Озвучить распознанный вызов. В бесшумной проверке не звучит.",
        "Обратная связь"
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
