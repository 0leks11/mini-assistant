use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// Main configuration — loaded from config.toml (not in git!)
/// Copy config.example.toml → config.toml and fill in values
#[derive(Debug, Deserialize)]
pub struct Config {
    pub assistant: AssistantConfig,
    pub gemini: Option<GeminiConfig>,
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
    /// Languages to understand: ["ru-RU", "en-US"]
    #[allow(dead_code)]
    pub languages: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceProvider {
    #[default]
    Gemini,
}

impl VoiceProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini Live",
        }
    }

    pub fn host(self) -> &'static str {
        match self {
            Self::Gemini => "generativelanguage.googleapis.com:443",
        }
    }

    pub fn key_env(self) -> &'static str {
        match self {
            Self::Gemini => "GEMINI_API_KEY",
        }
    }
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

impl Config {
    pub fn voice_settings(&self) -> Result<VoiceSettings<'_>> {
        let provider = self.assistant.provider;
        let (model, voice, configured_key) = match provider {
            VoiceProvider::Gemini => {
                let config = self.gemini.as_ref().context("Missing [gemini] section")?;
                (
                    &config.model,
                    &self.assistant.voice,
                    config.api_key.as_deref(),
                )
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
}
