//! Persistent credential store (`credentials.json`) with 0600 permissions,
//! atomic writes, and backward-compatible fallback to `config.toml` and `.env`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::{Config, SpotifyConfig, VoiceProvider};

pub const DEFAULT_DEVICE_CREDENTIALS_DIR: &str = "/bkupgrade/data/opt/mini-assistant";
pub const FALLBACK_DEVICE_CREDENTIALS_DIR: &str = "/data/opt/mini-assistant";
pub const CREDENTIALS_FILENAME: &str = "credentials.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    ApiKey,
    Oauth,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Oauth => "oauth",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderCredentials {
    pub auth_mode: AuthMode,
    pub api_key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub expires_at: Option<u64>,
    pub account_label: Option<String>,
}

impl ProviderCredentials {
    pub fn active_secret(&self) -> Option<&str> {
        match self.auth_mode {
            AuthMode::Oauth => self
                .access_token
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    self.api_key
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                }),
            AuthMode::ApiKey => self
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    self.access_token
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                }),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SpotifyCredentials {
    pub client_id: Option<String>,
    pub refresh_token: Option<String>,
    pub account_label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CredentialsFile {
    pub gemini: Option<ProviderCredentials>,
    pub openai: Option<ProviderCredentials>,
    pub spotify: Option<SpotifyCredentials>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVoiceCredential {
    pub secret: String,
    pub auth_mode: AuthMode,
    pub source: &'static str,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VoiceRuntimeState {
    pub status: String,
    pub detail: Option<String>,
}

impl VoiceRuntimeState {
    pub fn new(status: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            status: status.into(),
            detail,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderAuthSummary {
    pub configured: bool,
    pub auth_mode: Option<String>,
    pub source: Option<String>,
    pub masked_hint: Option<String>,
    pub account_label: Option<String>,
    pub expires_at: Option<u64>,
    pub has_refresh_token: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpotifyAuthSummary {
    pub client_id_set: bool,
    pub has_refresh_token: bool,
    pub account_label: Option<String>,
    pub masked_client_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllAuthSummary {
    pub gemini: ProviderAuthSummary,
    pub openai: ProviderAuthSummary,
    pub spotify: SpotifyAuthSummary,
}

/// Mask a secret safely so raw keys/tokens never appear in HTTP responses or logs.
pub fn mask_secret(secret: &str) -> String {
    let s = secret.trim();
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 10 {
        return "***".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

/// Resolve the path to `credentials.json` without colliding with tests or legacy deployments.
pub fn resolve_credentials_path(config_path: Option<&Path>) -> PathBuf {
    if let Ok(override_path) = std::env::var("MINI_CREDENTIALS_FILE") {
        let trimmed = override_path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    if let Some(cfg) = config_path {
        let is_default_device_cfg = cfg == Path::new("/data/opt/mini-assistant/config.toml");
        if !is_default_device_cfg {
            if let Some(parent) = cfg.parent() {
                if !parent.as_os_str().is_empty() {
                    return parent.join(CREDENTIALS_FILENAME);
                }
            }
        }
    }

    let persistent_dir = Path::new(DEFAULT_DEVICE_CREDENTIALS_DIR);
    if persistent_dir.exists() {
        return persistent_dir.join(CREDENTIALS_FILENAME);
    }

    let device_dir = Path::new(FALLBACK_DEVICE_CREDENTIALS_DIR);
    if device_dir.exists() {
        return device_dir.join(CREDENTIALS_FILENAME);
    }

    PathBuf::from(CREDENTIALS_FILENAME)
}

#[derive(Debug, Clone)]
pub struct CredentialsStore {
    path: PathBuf,
}

impl CredentialsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn for_config(config_path: Option<&Path>) -> Self {
        Self::new(resolve_credentials_path(config_path))
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<CredentialsFile> {
        match std::fs::read_to_string(&self.path) {
            Ok(content) => {
                if content.trim().is_empty() {
                    return Ok(CredentialsFile::default());
                }
                serde_json::from_str(&content).with_context(|| {
                    format!(
                        "Failed to parse credentials JSON at {}",
                        self.path.display()
                    )
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CredentialsFile::default()),
            Err(e) => Err(e).with_context(|| {
                format!("Failed to read credentials file {}", self.path.display())
            }),
        }
    }

    pub fn save(&self, data: &CredentialsFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "Failed to create credentials directory {}",
                        parent.display()
                    )
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
        }

        let payload =
            serde_json::to_vec_pretty(data).context("Failed to serialize credentials store")?;
        let tmp_path = self
            .path
            .with_extension(format!("json.tmp.{}", std::process::id()));

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .with_context(|| {
                    format!("Failed to open temp credentials {}", tmp_path.display())
                })?;
            file.write_all(&payload)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        {
            let mut bytes = payload;
            bytes.push(b'\n');
            std::fs::write(&tmp_path, &bytes)?;
        }

        std::fs::rename(&tmp_path, &self.path).with_context(|| {
            format!(
                "Failed to atomically commit credentials file {}",
                self.path.display()
            )
        })?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn set_provider(&self, provider: VoiceProvider, creds: ProviderCredentials) -> Result<()> {
        let mut file = self.load()?;
        match provider {
            VoiceProvider::Gemini => file.gemini = Some(creds),
            VoiceProvider::Openai => file.openai = Some(creds),
        }
        self.save(&file)
    }

    #[allow(dead_code)]
    pub fn clear_provider(&self, provider: VoiceProvider) -> Result<()> {
        let mut file = self.load()?;
        match provider {
            VoiceProvider::Gemini => file.gemini = None,
            VoiceProvider::Openai => file.openai = None,
        }
        self.save(&file)
    }

    #[allow(dead_code)]
    pub fn set_spotify(&self, creds: SpotifyCredentials) -> Result<()> {
        let mut file = self.load()?;
        file.spotify = Some(creds);
        self.save(&file)
    }

    #[allow(dead_code)]
    pub fn clear_spotify(&self) -> Result<()> {
        let mut file = self.load()?;
        file.spotify = None;
        self.save(&file)
    }

    /// Resolve active credential for `provider`:
    /// 1) `credentials.json`
    /// 2) `config.toml`
    /// 3) `.env` / environment variable (`GEMINI_API_KEY` or `OPENAI_API_KEY`)
    pub fn resolve_voice_credential(
        &self,
        provider: VoiceProvider,
        config_key: Option<&str>,
    ) -> Option<ResolvedVoiceCredential> {
        if let Ok(file) = self.load() {
            let stored = match provider {
                VoiceProvider::Gemini => file.gemini.as_ref(),
                VoiceProvider::Openai => file.openai.as_ref(),
            };
            if let Some(entry) = stored {
                if let Some(secret) = entry.active_secret() {
                    return Some(ResolvedVoiceCredential {
                        secret: secret.to_string(),
                        auth_mode: entry.auth_mode,
                        source: "credentials.json",
                        refresh_token: entry.refresh_token.clone(),
                        client_id: entry.client_id.clone(),
                        expires_at: entry.expires_at,
                    });
                }
            }
        }

        if let Some(k) = config_key.map(str::trim).filter(|s| !s.is_empty()) {
            return Some(ResolvedVoiceCredential {
                secret: k.to_string(),
                auth_mode: AuthMode::ApiKey,
                source: "config.toml",
                refresh_token: None,
                client_id: None,
                expires_at: None,
            });
        }

        if let Ok(env_k) = std::env::var(provider.key_env()) {
            let trimmed = env_k.trim();
            if !trimmed.is_empty() {
                return Some(ResolvedVoiceCredential {
                    secret: trimmed.to_string(),
                    auth_mode: AuthMode::ApiKey,
                    source: "env",
                    refresh_token: None,
                    client_id: None,
                    expires_at: None,
                });
            }
        }

        None
    }

    /// Resolve Spotify Client ID and Refresh Token with backward compatibility.
    pub fn resolve_spotify_credentials(
        &self,
        spotify_cfg: &SpotifyConfig,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let file = self.load().ok();
        let sp_stored = file.as_ref().and_then(|f| f.spotify.as_ref());

        let client_id = sp_stored
            .and_then(|s| s.client_id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                spotify_cfg
                    .client_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| {
                std::env::var("SPOTIFY_CLIENT_ID")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            });

        let refresh_token = sp_stored
            .and_then(|s| s.refresh_token.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                spotify_cfg
                    .refresh_token
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| {
                std::env::var("SPOTIFY_REFRESH_TOKEN")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| crate::spotify::load_persisted_token(&spotify_cfg.token_file));

        let label = sp_stored.and_then(|s| s.account_label.clone());
        (client_id, refresh_token, label)
    }

    pub fn auth_summary(&self, config: Option<&Config>) -> AllAuthSummary {
        let stored = self.load().ok();
        let gemini_cfg_key = config.and_then(|c| c.gemini.as_ref()?.api_key.as_deref());
        let openai_cfg_key = config.and_then(|c| c.openai.as_ref()?.api_key.as_deref());

        let summarize = |provider: VoiceProvider, cfg_key: Option<&str>| -> ProviderAuthSummary {
            let label = stored.as_ref().and_then(|f| match provider {
                VoiceProvider::Gemini => f.gemini.as_ref()?.account_label.clone(),
                VoiceProvider::Openai => f.openai.as_ref()?.account_label.clone(),
            });
            match self.resolve_voice_credential(provider, cfg_key) {
                Some(resolved) => ProviderAuthSummary {
                    configured: true,
                    auth_mode: Some(resolved.auth_mode.as_str().to_string()),
                    source: Some(resolved.source.to_string()),
                    masked_hint: Some(mask_secret(&resolved.secret)),
                    account_label: label,
                    expires_at: resolved.expires_at,
                    has_refresh_token: resolved
                        .refresh_token
                        .as_deref()
                        .is_some_and(|r| !r.trim().is_empty()),
                },
                None => ProviderAuthSummary {
                    configured: false,
                    auth_mode: None,
                    source: None,
                    masked_hint: None,
                    account_label: label,
                    expires_at: None,
                    has_refresh_token: false,
                },
            }
        };

        let sp_cfg = config
            .and_then(|c| c.spotify.as_ref())
            .cloned()
            .unwrap_or_default();
        let (sp_id, sp_rt, sp_label) = self.resolve_spotify_credentials(&sp_cfg);

        AllAuthSummary {
            gemini: summarize(VoiceProvider::Gemini, gemini_cfg_key),
            openai: summarize(VoiceProvider::Openai, openai_cfg_key),
            spotify: SpotifyAuthSummary {
                client_id_set: sp_id.is_some(),
                has_refresh_token: sp_rt.is_some(),
                account_label: sp_label,
                masked_client_id: sp_id.as_deref().map(mask_secret),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_secret_never_exposes_full_value() {
        assert_eq!(mask_secret("short"), "***");
        assert_eq!(mask_secret("sk-proj-abcdef123456"), "sk-p...3456");
    }

    #[test]
    fn test_credentials_store_atomic_write_permissions_and_priority() {
        let dir = std::env::temp_dir().join(format!("mini_creds_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        let store = CredentialsStore::new(cred_path.clone());

        // Initially absent -> loads empty without error
        let empty = store.load().unwrap();
        assert_eq!(empty, CredentialsFile::default());

        // Save OpenAI OAuth credentials
        store
            .set_provider(
                VoiceProvider::Openai,
                ProviderCredentials {
                    auth_mode: AuthMode::Oauth,
                    api_key: None,
                    access_token: Some("oauth-access-token-123456".to_string()),
                    refresh_token: Some("oauth-refresh-token-xyz".to_string()),
                    client_id: Some("oaiapp_test".to_string()),
                    expires_at: Some(1_800_000_000),
                    account_label: Some("user@example.com".to_string()),
                },
            )
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cred_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials.json must enforce 0600 mode");
        }

        // Resolution prefers credentials.json over config_key fallback
        let resolved = store
            .resolve_voice_credential(VoiceProvider::Openai, Some("sk-config-fallback-0000"))
            .unwrap();
        assert_eq!(resolved.secret, "oauth-access-token-123456");
        assert_eq!(resolved.auth_mode, AuthMode::Oauth);
        assert_eq!(resolved.source, "credentials.json");

        // Auth summary hides raw tokens
        let summary = store.auth_summary(None);
        assert!(summary.openai.configured);
        assert_eq!(summary.openai.auth_mode.as_deref(), Some("oauth"));
        assert_eq!(summary.openai.masked_hint.as_deref(), Some("oaut...3456"));
        assert!(!summary
            .openai
            .masked_hint
            .as_deref()
            .unwrap()
            .contains("123456"));
        assert!(summary.openai.has_refresh_token);

        // Logout clears credentials.json entry and falls back cleanly
        store.clear_provider(VoiceProvider::Openai).unwrap();
        let fallback = store
            .resolve_voice_credential(VoiceProvider::Openai, Some("sk-config-fallback-0000"))
            .unwrap();
        assert_eq!(fallback.secret, "sk-config-fallback-0000");
        assert_eq!(fallback.source, "config.toml");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_credentials_path_respects_custom_config_dir() {
        let custom_cfg = PathBuf::from("/tmp/some_test_dir/config.toml");
        let resolved = resolve_credentials_path(Some(&custom_cfg));
        assert_eq!(
            resolved,
            PathBuf::from("/tmp/some_test_dir/credentials.json")
        );
    }
}
