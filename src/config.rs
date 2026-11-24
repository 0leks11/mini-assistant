use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

use crate::audio::AecConfig;

/// Main configuration — loaded from config.toml (not in git!)
/// Copy config.example.toml → config.toml and fill in values
#[derive(Debug, Deserialize)]
pub struct Config {
    pub assistant: AssistantConfig,
    pub gemini: Option<GeminiConfig>,
    #[serde(default)]
    pub openai: Option<OpenAiConfig>,
    pub audio: AudioConfig,
    pub ha: Option<HomeAssistantConfig>,
    #[serde(default)]
    #[allow(dead_code)]
    pub weather: Option<WeatherConfig>,
    #[serde(default)]
    pub alarms: Option<AlarmConfig>,
    #[serde(default)]
    pub spotify: Option<SpotifyConfig>,
    #[serde(default)]
    pub wifi: Option<WifiConfig>,
    #[serde(default)]
    pub wakeword: Option<WakewordCalibrationConfig>,
}

#[derive(Debug, Deserialize)]
pub struct AssistantConfig {
    /// Display name of the assistant
    pub name: String,
    #[serde(default)]
    pub provider: VoiceProvider,
    /// Gemini voice to use for responses
    #[serde(default)]
    pub voice: String,
    /// Custom wake words (e.g. ["штучка", "штучка-интеллект"])
    #[allow(dead_code)]
    #[serde(default)]
    pub wake_words: Vec<String>,
    /// Private, locally enrolled Rustpotter reference; absent means manual activation only.
    #[serde(default)]
    pub wakeword_model: Option<PathBuf>,
    /// Whether acoustic wake-word detection is enabled (default: true).
    /// If false, the speaker is completely deaf to all acoustic triggers.
    #[serde(default = "default_true")]
    pub wakeword_enabled: bool,
    /// Whether to play a soft acoustic earcon chime upon wake-word trigger (default: false).
    #[serde(default)]
    pub wake_chime: bool,
    /// Default top ring LED color ("azure", "purple", "emerald", "amber", "white", "off")
    #[serde(default = "default_led_color")]
    pub led_color: String,
    /// Languages to understand: ["ru-RU", "en-US"]
    #[allow(dead_code)]
    pub languages: Vec<String>,
}

fn default_led_color() -> String {
    "azure".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceProvider {
    #[default]
    Gemini,
    Openai,
}

impl VoiceProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini Live",
            Self::Openai => "OpenAI Realtime",
        }
    }

    pub fn host(self) -> &'static str {
        match self {
            Self::Gemini => "generativelanguage.googleapis.com:443",
            Self::Openai => "api.openai.com:443",
        }
    }

    pub fn key_env(self) -> &'static str {
        match self {
            Self::Gemini => "GEMINI_API_KEY",
            Self::Openai => "OPENAI_API_KEY",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiConfig {
    #[serde(default = "default_openai_model")]
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

fn default_openai_model() -> String {
    "gpt-4o-realtime-preview".to_string()
}

#[derive(Debug, Deserialize)]
pub struct GeminiConfig {
    /// Gemini model for Live API
    pub model: String,
    /// API key — loaded from env GEMINI_API_KEY if not set here
    #[serde(default)]
    pub api_key: Option<String>,
}

pub struct VoiceSettings<'a> {
    pub provider: VoiceProvider,
    pub model: &'a str,
    pub voice: &'a str,
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AudioConfig {
    /// ALSA capture device (microphone)
    #[allow(dead_code)]
    pub capture_device: String,
    /// ALSA playback device (speaker)
    pub playback_device: String,
    /// Sample rate for capture (Hz)
    #[allow(dead_code)]
    pub sample_rate: u32,
    /// Channels for capture
    #[allow(dead_code)]
    pub channels: u16,
    /// Capture backend: "vqe" (vendor IPC :9850, default), "alsa" (direct hw:0,2), "mock"
    #[serde(default = "default_capture_backend")]
    pub capture_backend: String,
    /// Acoustic Echo Cancellation (MDF AEC) configuration
    #[serde(default)]
    pub aec: AecConfig,
}

fn default_capture_backend() -> String {
    "alsa".to_string()
}

#[derive(Debug, Deserialize)]
pub struct HomeAssistantConfig {
    /// Home Assistant URL e.g. http://192.168.1.x:8123
    pub url: String,
    /// Long-lived access token — loaded from env HA_TOKEN if not set
    #[serde(default)]
    pub token: Option<String>,
    /// Explicit local policy. Empty by default: no cloud-requested HA tools.
    #[serde(default)]
    pub allowed_entities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WeatherConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_weather_provider")]
    pub provider: String,
    #[serde(default = "default_weather_location")]
    pub default_location: String,
    #[serde(default = "default_weather_cache_ttl")]
    pub cache_ttl_minutes: u64,
}

fn default_weather_provider() -> String {
    "open-meteo".to_string()
}

fn default_weather_location() -> String {
    "batumi".to_string()
}

fn default_weather_cache_ttl() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlarmConfig {
    #[serde(default = "default_alarm_enabled")]
    pub enabled: bool,
    #[serde(default = "default_alarm_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_alarm_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub melody_path: Option<PathBuf>,
    #[serde(default = "default_alarm_max_duration_secs")]
    pub max_duration_secs: u64,
    #[serde(default = "default_alarm_volume_ramp_secs")]
    pub volume_ramp_secs: u64,
}

fn default_alarm_enabled() -> bool {
    false
}

fn default_alarm_data_dir() -> PathBuf {
    PathBuf::from("/bkupgrade/data/opt/mini-assistant/alarms")
}

fn default_alarm_timezone() -> String {
    "Asia/Tbilisi".to_string()
}

fn default_alarm_max_duration_secs() -> u64 {
    300
}

fn default_alarm_volume_ramp_secs() -> u64 {
    20
}

impl Default for AlarmConfig {
    fn default() -> Self {
        Self {
            enabled: default_alarm_enabled(),
            data_dir: default_alarm_data_dir(),
            timezone: default_alarm_timezone(),
            melody_path: None,
            max_duration_secs: default_alarm_max_duration_secs(),
            volume_ramp_secs: default_alarm_volume_ramp_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotifyConfig {
    #[serde(default = "default_spotify_enabled")]
    pub enabled: bool,
    #[serde(default = "default_spotify_soloist_ctl")]
    pub soloist_ctl: PathBuf,
    #[serde(default = "default_spotify_ws_url")]
    pub ws_url: String,
    #[serde(default = "default_spotify_web_port")]
    pub web_port: u16,
    pub client_id: Option<String>,
    pub refresh_token: Option<String>,
    #[serde(default = "default_spotify_token_file")]
    pub token_file: PathBuf,
    #[allow(dead_code)]
    #[serde(default = "default_spotify_device_name")]
    pub device_name: String,
}

fn default_spotify_enabled() -> bool {
    true
}

fn default_spotify_soloist_ctl() -> PathBuf {
    PathBuf::from("/data/opt/mini-assistant/soloist-ctl")
}

fn default_spotify_ws_url() -> String {
    "127.0.0.1:9090".to_string()
}

fn default_spotify_web_port() -> u16 {
    8080
}

fn default_spotify_token_file() -> PathBuf {
    PathBuf::from("/bkupgrade/data/opt/mini-assistant/spotify_token.txt")
}

fn default_spotify_device_name() -> String {
    "GPT Mini".to_string()
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            enabled: default_spotify_enabled(),
            soloist_ctl: default_spotify_soloist_ctl(),
            ws_url: default_spotify_ws_url(),
            web_port: default_spotify_web_port(),
            client_id: None,
            refresh_token: None,
            token_file: default_spotify_token_file(),
            device_name: default_spotify_device_name(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WifiConfig {
    #[serde(default = "default_wifi_enabled")]
    pub enabled: bool,
    #[serde(default = "default_wifi_interface")]
    pub interface: String,
    #[serde(default = "default_wifi_dns_port")]
    pub dns_port: u16,
    #[serde(default = "default_wifi_portal_port")]
    pub portal_port: u16,
}

fn default_wifi_enabled() -> bool {
    true
}

fn default_wifi_interface() -> String {
    "wlan0".to_string()
}

fn default_wifi_dns_port() -> u16 {
    53
}

fn default_wifi_portal_port() -> u16 {
    80
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            enabled: default_wifi_enabled(),
            interface: default_wifi_interface(),
            dns_port: default_wifi_dns_port(),
            portal_port: default_wifi_portal_port(),
        }
    }
}

/// User-adjustable acoustic parameters. Scores are detector units, not probabilities.
/// Missing fields retain backwards-compatible defaults; invalid values are rejected.
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WakewordCalibrationConfig {
    pub threshold: f32,
    pub avg_threshold: f32,
    pub min_scores: usize,
    pub score_mode: String,
    pub eager: bool,
    pub vad_threshold: f32,
    pub gain_normalizer: bool,
    pub band_pass: bool,
    pub enabled: bool,
    pub shadow_mode: bool,
    /// Applied after local detection, to the authorized cloud audio only.
    pub mic_gain: f32,
    pub pre_roll_ms: u32,
    pub silence_timeout_ms: u32,
    pub initial_silence_timeout_ms: u32,
    pub max_utterance_secs: u64,
    pub echo_tail_ms: u64,
    pub startup_suppression_ms: u32,
    pub manual_activation_secs: u64,
    pub wake_chime: bool,
    pub followup_timeout_secs: u64,
    pub vad_noise_adaptation: bool,
    pub vad_noise_multiplier: f32,
    pub vad_noise_offset: f32,
    pub vad_rise_alpha: f32,
    pub vad_fall_alpha: f32,
    pub vad_onset_frames: usize,
    pub gain_min: f32,
    pub gain_max: f32,
    pub gain_ref: Option<f32>,
    pub band_low_hz: f32,
    pub band_high_hz: f32,
    pub score_ref: f32,
    pub dtw_band_size: u16,
}

impl Default for WakewordCalibrationConfig {
    fn default() -> Self {
        Self {
            threshold: default_ww_threshold(),
            avg_threshold: default_ww_avg_threshold(),
            min_scores: default_ww_min_scores(),
            score_mode: default_ww_score_mode(),
            eager: true,
            vad_threshold: default_vad_threshold(),
            gain_normalizer: true,
            band_pass: false,
            enabled: true,
            shadow_mode: false,
            mic_gain: 1.0,
            pre_roll_ms: 400,
            silence_timeout_ms: 1100,
            initial_silence_timeout_ms: 1800,
            max_utterance_secs: 8,
            echo_tail_ms: 800,
            startup_suppression_ms: 2000,
            manual_activation_secs: 8,
            wake_chime: false,
            followup_timeout_secs: 0,
            vad_noise_adaptation: true,
            vad_noise_multiplier: 1.15,
            vad_noise_offset: 10.0,
            vad_rise_alpha: 0.02,
            vad_fall_alpha: 0.10,
            vad_onset_frames: 3,
            gain_min: 0.1,
            gain_max: 6.0,
            gain_ref: None,
            band_low_hz: 80.0,
            band_high_hz: 7500.0,
            score_ref: 0.22,
            dtw_band_size: 5,
        }
    }
}

impl WakewordCalibrationConfig {
    /// Validate the complete settings at the configuration/API boundary.
    /// No setting is silently clamped; an error leaves the running settings intact.
    pub fn validate(&self) -> Result<()> {
        fn finite_range(name: &str, value: f32, min: f32, max: f32) -> Result<()> {
            anyhow::ensure!(
                value.is_finite() && (min..=max).contains(&value),
                "{name} must be finite and within {min}..={max}"
            );
            Ok(())
        }
        finite_range("threshold", self.threshold, 0.0, 0.7311)?;
        finite_range("avg_threshold", self.avg_threshold, 0.0, 0.7311)?;
        finite_range("vad_threshold", self.vad_threshold, 1.0, 1000.0)?;
        finite_range("mic_gain", self.mic_gain, 0.1, 20.0)?;
        finite_range("vad_noise_multiplier", self.vad_noise_multiplier, 1.0, 10.0)?;
        finite_range("vad_noise_offset", self.vad_noise_offset, 0.0, 1000.0)?;
        finite_range(
            "vad_rise_alpha",
            self.vad_rise_alpha,
            f32::MIN_POSITIVE,
            1.0,
        )?;
        finite_range(
            "vad_fall_alpha",
            self.vad_fall_alpha,
            f32::MIN_POSITIVE,
            1.0,
        )?;
        finite_range("gain_min", self.gain_min, 0.1, 20.0)?;
        finite_range("gain_max", self.gain_max, 0.1, 20.0)?;
        anyhow::ensure!(
            self.gain_min <= self.gain_max,
            "gain_min must not exceed gain_max"
        );
        if let Some(gain_ref) = self.gain_ref {
            finite_range("gain_ref", gain_ref, f32::MIN_POSITIVE, 1.0)?;
        }
        finite_range("band_low_hz", self.band_low_hz, 20.0, 7900.0)?;
        finite_range("band_high_hz", self.band_high_hz, 20.0, 7900.0)?;
        anyhow::ensure!(
            self.band_low_hz < self.band_high_hz,
            "band_low_hz must be below band_high_hz"
        );
        finite_range("score_ref", self.score_ref, 0.01, 2.0)?;
        anyhow::ensure!(
            (1..=100).contains(&self.min_scores),
            "min_scores must be within 1..=100"
        );
        anyhow::ensure!(
            (1..=100).contains(&self.vad_onset_frames),
            "vad_onset_frames must be within 1..=100"
        );
        anyhow::ensure!(
            self.pre_roll_ms <= 2000,
            "pre_roll_ms must be within 0..=2000"
        );
        anyhow::ensure!(
            (200..=10000).contains(&self.silence_timeout_ms),
            "silence_timeout_ms must be within 200..=10000"
        );
        anyhow::ensure!(
            (500..=15000).contains(&self.initial_silence_timeout_ms),
            "initial_silence_timeout_ms must be within 500..=15000"
        );
        anyhow::ensure!(
            (3..=60).contains(&self.max_utterance_secs),
            "max_utterance_secs must be within 3..=60"
        );
        anyhow::ensure!(
            self.echo_tail_ms <= 5000,
            "echo_tail_ms must be within 0..=5000"
        );
        anyhow::ensure!(
            self.startup_suppression_ms <= 10000,
            "startup_suppression_ms must be within 0..=10000"
        );
        anyhow::ensure!(
            (1..=30).contains(&self.manual_activation_secs),
            "manual_activation_secs must be within 1..=30"
        );
        anyhow::ensure!(
            self.followup_timeout_secs <= 15,
            "followup_timeout_secs must be within 0..=15"
        );
        anyhow::ensure!(
            (1..=32).contains(&self.dtw_band_size),
            "dtw_band_size must be within 1..=32"
        );
        anyhow::ensure!(
            matches!(
                self.score_mode.trim().to_ascii_lowercase().as_str(),
                "max"
                    | "avg"
                    | "average"
                    | "median"
                    | "p25"
                    | "p50"
                    | "p75"
                    | "p80"
                    | "p90"
                    | "p95"
            ),
            "Unknown score_mode"
        );
        Ok(())
    }

    /// Convert validated settings to the local personal-reference detector.
    pub fn to_detector_config(&self) -> rustpotter::PersonalDetectorConfig {
        rustpotter::PersonalDetectorConfig {
            threshold: self.threshold,
            avg_threshold: self.avg_threshold,
            min_scores: self.min_scores,
            score_mode: match self.score_mode.trim().to_ascii_lowercase().as_str() {
                "median" | "p50" => rustpotter::ScoreMode::Median,
                "average" | "avg" => rustpotter::ScoreMode::Average,
                "p25" => rustpotter::ScoreMode::P25,
                "p75" => rustpotter::ScoreMode::P75,
                "p80" => rustpotter::ScoreMode::P80,
                "p90" => rustpotter::ScoreMode::P90,
                "p95" => rustpotter::ScoreMode::P95,
                _ => rustpotter::ScoreMode::Max,
            },
            gain_normalizer_enabled: self.gain_normalizer,
            band_pass_enabled: self.band_pass,
            eager: self.eager,
            gain_min: self.gain_min,
            gain_max: self.gain_max,
            gain_ref: self.gain_ref,
            band_low_hz: self.band_low_hz,
            band_high_hz: self.band_high_hz,
            score_ref: self.score_ref,
            dtw_band_size: self.dtw_band_size,
        }
    }
}

fn default_ww_threshold() -> f32 {
    0.38
}

fn default_ww_avg_threshold() -> f32 {
    0.19
}

fn default_ww_min_scores() -> usize {
    4
}

fn default_ww_score_mode() -> String {
    "max".to_string()
}

fn default_vad_threshold() -> f32 {
    45.0
}

impl Config {
    pub fn voice_settings(&self) -> Result<VoiceSettings<'_>> {
        let provider = self.assistant.provider;
        let (model, voice, configured_key) = match provider {
            VoiceProvider::Gemini => {
                let config = self.gemini.as_ref().context("Missing [gemini] section")?;
                (
                    config.model.as_str(),
                    self.assistant.voice.as_str(),
                    config.api_key.as_deref(),
                )
            }
            VoiceProvider::Openai => {
                let model = self
                    .openai
                    .as_ref()
                    .map(|o| o.model.as_str())
                    .unwrap_or("gpt-4o-realtime-preview");
                let voice = if self.assistant.voice.trim().is_empty() {
                    "alloy"
                } else {
                    self.assistant.voice.as_str()
                };
                let key = self.openai.as_ref().and_then(|o| o.api_key.as_deref());
                (model, voice, key)
            }
        };
        anyhow::ensure!(
            !model.trim().is_empty() && !voice.trim().is_empty(),
            "Voice model and voice must be nonempty"
        );
        let api_key = configured_key
            .map(str::to_owned)
            .or_else(|| std::env::var(provider.key_env()).ok())
            .filter(|key| !key.trim().is_empty());
        Ok(VoiceSettings {
            provider,
            model,
            voice,
            api_key,
        })
    }

    /// Load config from file, falling back to env vars for secrets
    #[allow(dead_code)]
    pub fn load() -> Result<Self> {
        Self::load_from(None)
    }

    /// Load config from an explicit path or standard candidate paths
    pub fn load_from(path: Option<&std::path::Path>) -> Result<Self> {
        let (config_path, config_str) = if let Some(p) = path {
            (
                p.to_path_buf(),
                std::fs::read_to_string(p)
                    .with_context(|| format!("Failed to read config from {}", p.display()))?,
            )
        } else if std::path::Path::new("config.toml").exists() {
            (
                PathBuf::from("config.toml"),
                std::fs::read_to_string("config.toml").context("Failed to read config.toml")?,
            )
        } else if std::path::Path::new("/data/opt/mini-assistant/config.toml").exists() {
            (
                PathBuf::from("/data/opt/mini-assistant/config.toml"),
                std::fs::read_to_string("/data/opt/mini-assistant/config.toml")
                    .context("Failed to read /data/opt/mini-assistant/config.toml")?,
            )
        } else {
            // Use defaults for testing
            tracing::warn!("config.toml not found, using defaults");
            (
                PathBuf::from("defaults"),
                include_str!("../config.example.toml").to_string(),
            )
        };

        let mut config: Config = toml::from_str(&config_str)
            .with_context(|| format!("Failed to parse config from {}", config_path.display()))?;

        // Override with env vars (secrets should come from env, not config file)
        if let Some(ref mut ha) = config.ha {
            if let Ok(url) = std::env::var("HA_URL") {
                let trimmed = url.trim();
                if !trimmed.is_empty() && !trimmed.contains(".x") {
                    ha.url = trimmed.to_string();
                }
            }
            if ha.token.is_none() {
                ha.token = std::env::var("HA_TOKEN").ok();
            }
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_parse_example_toml() {
        let example_toml = include_str!("../config.example.toml");
        let config: Config =
            toml::from_str(example_toml).expect("config.example.toml must be valid TOML");
        assert_eq!(config.assistant.name, "Мини");
        assert_eq!(config.assistant.voice, "Aoede");
        assert_eq!(
            config.assistant.wake_words,
            vec!["штучка", "штучка-интеллект"]
        );
        assert!(config.assistant.wakeword_enabled);
        assert_eq!(config.assistant.provider, VoiceProvider::Gemini);
        let gemini = config.gemini.unwrap();
        assert_eq!(gemini.model, "models/gemini-3.8-live");
        assert!(gemini.api_key.is_none());
        assert!(config.ha.is_some());
    }

    #[test]
    fn test_config_ha_url_parses_without_environment_overrides() {
        let toml_str = r#"
            [assistant]
            name = "Тест"
            voice = "Puck"
            languages = ["ru-RU"]

            [gemini]
            model = "models/gemini-3.8-live"

            [audio]
            capture_device = "hw:0,0"
            playback_device = "hw:0,0"
            sample_rate = 16000
            channels = 1

            [ha]
            url = "http://192.168.77.2:8123"
        "#;

        let config: Config = toml::from_str(toml_str).unwrap();
        let ha = config.ha.unwrap();
        assert_eq!(ha.url, "http://192.168.77.2:8123");
        assert!(ha.allowed_entities.is_empty(), "HA tools must be opt-in");
    }

    #[test]
    fn test_config_wakeword_enabled_can_be_disabled() {
        let toml_str = r#"
            [assistant]
            name = "Тест"
            voice = "Puck"
            languages = ["ru-RU"]
            wakeword_enabled = false

            [audio]
            capture_device = "hw:0,0"
            playback_device = "hw:0,0"
            sample_rate = 16000
            channels = 1
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.assistant.wakeword_enabled);
    }

    #[test]
    fn test_spotify_and_wifi_config_defaults_and_parsing() {
        let toml_str = r#"
            [assistant]
            name = "Тест"
            voice = "Puck"
            languages = ["ru-RU"]

            [gemini]
            model = "models/gemini-3.8-live"

            [audio]
            capture_device = "hw:0,0"
            playback_device = "hw:0,0"
            sample_rate = 16000
            channels = 1

            [spotify]
            enabled = true
            web_port = 8080
            client_id = "test-client-id"

            [wifi]
            interface = "wlan0"
            dns_port = 1053
            portal_port = 8088
        "#;

        let config: Config = toml::from_str(toml_str).unwrap();
        let spotify = config.spotify.expect("spotify config must parse");
        assert!(spotify.enabled);
        assert_eq!(spotify.web_port, 8080);
        assert_eq!(spotify.client_id.as_deref(), Some("test-client-id"));
        assert!(spotify.refresh_token.is_none());

        let wifi = config.wifi.expect("wifi config must parse");
        assert!(wifi.enabled);
        assert_eq!(wifi.interface, "wlan0");
        assert_eq!(wifi.dns_port, 1053);
        assert_eq!(wifi.portal_port, 8088);
    }

    #[test]
    fn acoustic_legacy_profile_preserves_new_defaults() {
        let old = r#"
threshold = 0.40
avg_threshold = 0.21
min_scores = 7
score_mode = "median"
eager = false
vad_threshold = 55.0
gain_normalizer = false
band_pass = true
"#;
        let settings: WakewordCalibrationConfig = toml::from_str(old).unwrap();
        settings.validate().unwrap();
        assert_eq!(settings.threshold, 0.40);
        assert_eq!(settings.min_scores, 7);
        assert_eq!(settings.pre_roll_ms, 400);
        assert_eq!(settings.followup_timeout_secs, 0);
        assert_eq!(settings.band_high_hz, 7500.0);
        assert_eq!(settings.gain_ref, None);
    }

    #[test]
    fn acoustic_profile_roundtrip_preserves_non_default_values() {
        let settings = WakewordCalibrationConfig {
            enabled: false,
            shadow_mode: true,
            threshold: 0.52,
            avg_threshold: 0.24,
            min_scores: 13,
            score_mode: "p75".into(),
            eager: false,
            vad_threshold: 73.0,
            gain_normalizer: false,
            band_pass: true,
            gain_min: 0.2,
            gain_max: 8.0,
            gain_ref: Some(0.003),
            band_low_hz: 200.0,
            band_high_hz: 4000.0,
            score_ref: 0.31,
            dtw_band_size: 9,
            mic_gain: 2.0,
            pre_roll_ms: 650,
            silence_timeout_ms: 1400,
            initial_silence_timeout_ms: 2200,
            max_utterance_secs: 12,
            echo_tail_ms: 1200,
            startup_suppression_ms: 2500,
            manual_activation_secs: 10,
            wake_chime: true,
            followup_timeout_secs: 5,
            vad_noise_adaptation: false,
            vad_noise_multiplier: 2.0,
            vad_noise_offset: 15.0,
            vad_rise_alpha: 0.04,
            vad_fall_alpha: 0.2,
            vad_onset_frames: 5,
        };
        settings.validate().unwrap();
        let toml = toml::to_string(&settings).unwrap();
        let restored: WakewordCalibrationConfig = toml::from_str(&toml).unwrap();
        assert_eq!(restored, settings);
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            serde_json::from_str::<WakewordCalibrationConfig>(&json).unwrap(),
            settings
        );
    }

    #[test]
    fn acoustic_validation_rejects_nonfinite_and_unusable_values() {
        let base = WakewordCalibrationConfig::default();
        base.validate().unwrap();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.01, 0.8] {
            let mut cfg = base.clone();
            cfg.threshold = bad;
            assert!(cfg.validate().is_err());
        }
        for field in [
            "avg_threshold",
            "vad_threshold",
            "mic_gain",
            "gain_min",
            "gain_max",
            "band_low_hz",
            "band_high_hz",
            "score_ref",
            "vad_noise_multiplier",
            "vad_noise_offset",
            "vad_rise_alpha",
            "vad_fall_alpha",
        ] {
            let mut json = serde_json::to_value(&base).unwrap();
            json[field] = serde_json::json!("not-a-number");
            assert!(serde_json::from_value::<WakewordCalibrationConfig>(json).is_err());
        }
        let mut cfg = base.clone();
        cfg.gain_ref = Some(f32::NAN);
        assert!(cfg.validate().is_err());
        cfg = base.clone();
        cfg.gain_min = 8.0;
        cfg.gain_max = 6.0;
        assert!(cfg.validate().is_err());
        cfg = base.clone();
        cfg.band_low_hz = cfg.band_high_hz;
        assert!(cfg.validate().is_err());
        cfg = base.clone();
        cfg.score_mode = "mystery".into();
        assert!(cfg.validate().is_err());
        cfg = base.clone();
        cfg.vad_rise_alpha = 0.0;
        assert!(cfg.validate().is_err());
        cfg = base.clone();
        cfg.min_scores = 0;
        assert!(cfg.validate().is_err());
        cfg = base.clone();
        cfg.followup_timeout_secs = 16;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn acoustic_profiles_reject_unknown_fields_instead_of_ignoring_tuning() {
        assert!(toml::from_str::<WakewordCalibrationConfig>("threshold_typo = 0.4").is_err());
        assert!(
            serde_json::from_str::<WakewordCalibrationConfig>(r#"{"noise_supression":true}"#)
                .is_err()
        );
    }
}
