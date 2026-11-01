use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// Main configuration — loaded from config.toml (not in git!)
/// Copy config.example.toml → config.toml and fill in values
#[derive(Debug, Deserialize)]
pub struct Config {
    pub assistant: AssistantConfig,
    pub gemini: GeminiConfig,
    pub audio: AudioConfig,
    pub ha: Option<HomeAssistantConfig>,
}

#[derive(Debug, Deserialize)]
pub struct AssistantConfig {
    /// Display name of the assistant
    pub name: String,
    /// Gemini voice to use for responses
    pub voice: String,
    /// Languages to understand: ["ru-RU", "en-US"]
    pub languages: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GeminiConfig {
    /// Gemini model for Live API
    pub model: String,
    /// API key — loaded from env GEMINI_API_KEY if not set here
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AudioConfig {
    /// ALSA capture device (microphone)
    pub capture_device: String,
    /// ALSA playback device (speaker)
    pub playback_device: String,
    /// Sample rate for capture (Hz)
    pub sample_rate: u32,
    /// Channels for capture
    pub channels: u16,
}

#[derive(Debug, Deserialize)]
pub struct HomeAssistantConfig {
    /// Home Assistant URL e.g. http://192.168.1.x:8123
    pub url: String,
    /// Long-lived access token — loaded from env HA_TOKEN if not set
    #[serde(default)]
    pub token: Option<String>,
}

impl Config {
    /// Load config from file, falling back to env vars for secrets
    pub fn load() -> Result<Self> {
        let config_path = PathBuf::from("config.toml");

        let config_str = if config_path.exists() {
            std::fs::read_to_string(&config_path)
                .context("Failed to read config.toml")?
        } else {
            // Use defaults for testing
            tracing::warn!("config.toml not found, using defaults");
            include_str!("../config.example.toml").to_string()
        };

        let mut config: Config = toml::from_str(&config_str)
            .context("Failed to parse config.toml")?;

        // Override with env vars (secrets should come from env, not config file)
        if config.gemini.api_key.is_none() {
            config.gemini.api_key = std::env::var("GEMINI_API_KEY").ok();
        }
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
