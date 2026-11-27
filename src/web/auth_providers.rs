//! OAuth flows, device-code polling, token refresh, and pre-switch handshake
//! verification for OpenAI, Gemini, and Spotify credentials on GPT Mini.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::VoiceProvider;
use crate::credentials::{
    AuthMode, CredentialsStore, ProviderCredentials, ResolvedVoiceCredential,
};
use crate::gemini::{GeminiLiveClient, LiveClientConfig};
use crate::openai::OpenAiRealtimeClient;
use crate::spotify::crypto::pkce_code_challenge;

pub const DEFAULT_OPENAI_AUTH_BASE: &str = "https://auth.openai.com";
pub const DEFAULT_OPENAI_DEVICE_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEFAULT_OPENAI_DEVICE_VERIFY_URL: &str = "https://auth.openai.com/codex/device";

#[derive(Debug, Clone)]
pub struct PendingOAuthPkce {
    pub verifier: String,
    pub redirect_uri: String,
    pub client_id: String,
    pub created_at: Instant,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceAuthSession {
    pub session_id: String,
    pub client_id: String,
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval_secs: u64,
    #[serde(skip_serializing)]
    pub created_at: Instant,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePollOutcome {
    Pending {
        interval_secs: u64,
    },
    Authorized {
        account_label: Option<String>,
        realtime_verified: bool,
        realtime_detail: Option<String>,
    },
    Expired,
}

pub fn openai_auth_base_url(override_base: Option<&str>) -> String {
    override_base
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .or_else(|| {
            std::env::var("OPENAI_AUTH_BASE_URL")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_OPENAI_AUTH_BASE.to_string())
}

pub fn default_openai_client_id() -> String {
    std::env::var("OPENAI_OAUTH_CLIENT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OPENAI_DEVICE_CLIENT_ID.to_string())
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Deserialize)]
struct DeviceUserCodeResp {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(
        default = "default_poll_interval",
        deserialize_with = "deserialize_poll_interval"
    )]
    interval: u64,
    #[serde(default)]
    verification_uri: Option<String>,
}

fn default_poll_interval() -> u64 {
    5
}

fn deserialize_poll_interval<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::Number(n) => Ok(n.as_u64().unwrap_or_else(default_poll_interval)),
        serde_json::Value::String(s) => Ok(s
            .trim()
            .parse::<u64>()
            .unwrap_or_else(|_| default_poll_interval())),
        serde_json::Value::Null => Ok(default_poll_interval()),
        _ => Ok(default_poll_interval()),
    }
}

#[derive(Debug, Deserialize)]
struct DeviceTokenPollResp {
    #[serde(default)]
    authorization_code: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResp {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    id_token: Option<String>,
}

/// Start OpenAI Device Code Authorization flow (`POST /api/accounts/deviceauth/usercode`).
pub async fn start_openai_device_auth(
    client_id: Option<&str>,
    auth_base_override: Option<&str>,
) -> Result<DeviceAuthSession> {
    let base = openai_auth_base_url(auth_base_override);
    let cid = client_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_openai_client_id);

    let url = format!("{base}/api/accounts/deviceauth/usercode");
    let http = Client::builder().timeout(Duration::from_secs(12)).build()?;
    let resp = http
        .post(&url)
        .header("User-Agent", "GPTMini/1.0")
        .json(&serde_json::json!({ "client_id": cid }))
        .send()
        .await
        .context("Не удалось подключиться к серверу авторизации OpenAI")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        let brief = txt.chars().take(180).collect::<String>();
        anyhow::bail!("OpenAI device auth отклонён ({status}): {brief}");
    }

    let parsed: DeviceUserCodeResp = resp
        .json()
        .await
        .context("Некорректный ответ OpenAI usercode")?;

    let verify_url = parsed
        .verification_uri
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| {
            if base == DEFAULT_OPENAI_AUTH_BASE {
                DEFAULT_OPENAI_DEVICE_VERIFY_URL.to_string()
            } else {
                format!("{base}/codex/device")
            }
        });

    Ok(DeviceAuthSession {
        session_id: uuid::Uuid::new_v4().to_string(),
        client_id: cid,
        device_auth_id: parsed.device_auth_id,
        user_code: parsed.user_code,
        verification_url: verify_url,
        interval_secs: parsed.interval.clamp(3, 30),
        created_at: Instant::now(),
    })
}

/// Poll OpenAI Device Code endpoint once; exchanges code for OAuth tokens when user approves.
pub async fn poll_openai_device_auth(
    session: &DeviceAuthSession,
    store: &CredentialsStore,
    auth_base_override: Option<&str>,
    verify_endpoint_override: Option<&str>,
) -> Result<DevicePollOutcome> {
    if session.created_at.elapsed() > Duration::from_secs(900) {
        return Ok(DevicePollOutcome::Expired);
    }

    let base = openai_auth_base_url(auth_base_override);
    let poll_url = format!("{base}/api/accounts/deviceauth/token");
    let http = Client::builder().timeout(Duration::from_secs(12)).build()?;

    let resp = http
        .post(&poll_url)
        .header("User-Agent", "GPTMini/1.0")
        .json(&serde_json::json!({
            "device_auth_id": session.device_auth_id,
            "user_code": session.user_code,
        }))
        .send()
        .await
        .context("Ошибка опроса OpenAI device auth")?;

    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::NOT_FOUND
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::BAD_REQUEST
    {
        let body = resp.text().await.unwrap_or_default();
        let lower = body.to_ascii_lowercase();
        if lower.contains("expired") {
            return Ok(DevicePollOutcome::Expired);
        }
        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::NOT_FOUND
            || lower.contains("pending")
            || lower.contains("slow_down")
        {
            return Ok(DevicePollOutcome::Pending {
                interval_secs: session.interval_secs,
            });
        }
        anyhow::bail!(
            "Ошибка OpenAI device auth ({status}): {}",
            body.chars().take(180).collect::<String>()
        );
    }

    if !status.is_success() {
        anyhow::bail!("Неожиданный ответ OpenAI device auth: {status}");
    }

    let poll_data: DeviceTokenPollResp = resp
        .json()
        .await
        .context("Некорректный JSON от OpenAI deviceauth/token")?;

    let (access_token, refresh_token, expires_in, account_label) =
        if let Some(tok) = poll_data.access_token {
            (tok, poll_data.refresh_token, poll_data.expires_in, None)
        } else {
            let auth_code = poll_data
                .authorization_code
                .context("Отсутствует authorization_code в ответе OpenAI")?;
            let verifier = poll_data
                .code_verifier
                .context("Отсутствует code_verifier в ответе OpenAI")?;
            let redirect_uri = format!("{base}/deviceauth/callback");
            exchange_openai_code_for_tokens(
                &base,
                &session.client_id,
                &auth_code,
                &redirect_uri,
                Some(&verifier),
            )
            .await?
        };

    let expires_at = expires_in.map(|secs| now_unix_secs().saturating_add(secs));
    store.set_provider(
        VoiceProvider::Openai,
        ProviderCredentials {
            auth_mode: AuthMode::Oauth,
            api_key: None,
            access_token: Some(access_token.clone()),
            refresh_token,
            client_id: Some(session.client_id.clone()),
            expires_at,
            account_label: account_label.clone(),
        },
    )?;

    let (realtime_verified, realtime_detail) = match verify_provider_credential(
        VoiceProvider::Openai,
        &access_token,
        "gpt-4o-realtime-preview",
        "alloy",
        verify_endpoint_override,
    )
    .await
    {
        Ok(()) => (true, None),
        Err(e) => (false, Some(format!("{e:#}"))),
    };

    Ok(DevicePollOutcome::Authorized {
        account_label,
        realtime_verified,
        realtime_detail,
    })
}

pub fn build_openai_pkce_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    auth_base_override: Option<&str>,
) -> (String, String, PendingOAuthPkce) {
    let base = openai_auth_base_url(auth_base_override);
    let state = uuid::Uuid::new_v4().to_string();
    let verifier = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let challenge = pkce_code_challenge(&verifier);

    let url = format!(
        "{base}/oauth/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&state={}&code_challenge_method=S256&code_challenge={}&resource={}",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
        urlencoding::encode("openid profile email offline_access chatgpt.tokens.use.direct"),
        urlencoding::encode(&state),
        urlencoding::encode(&challenge),
        urlencoding::encode("https://api.openai.com/v1"),
    );

    let pending = PendingOAuthPkce {
        verifier,
        redirect_uri: redirect_uri.to_string(),
        client_id: client_id.to_string(),
        created_at: Instant::now(),
    };
    (url, state, pending)
}

pub async fn exchange_openai_code_for_tokens(
    auth_base: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: Option<&str>,
) -> Result<(String, Option<String>, Option<u64>, Option<String>)> {
    let token_url = format!("{auth_base}/oauth/token");
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
    ];
    if let Some(v) = code_verifier {
        form.push(("code_verifier", v));
    }

    let http = Client::builder().timeout(Duration::from_secs(12)).build()?;
    let resp = http
        .post(&token_url)
        .form(&form)
        .send()
        .await
        .context("Ошибка обмена authorization_code на токен OpenAI")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "OpenAI отклонил обмен кода ({status}): {}",
            txt.chars().take(180).collect::<String>()
        );
    }

    let tok: OAuthTokenResp = resp
        .json()
        .await
        .context("Некорректный JSON от OpenAI oauth/token")?;

    let email = tok.id_token.as_deref().and_then(extract_email_from_jwt);
    Ok((tok.access_token, tok.refresh_token, tok.expires_in, email))
}

/// Automatically refresh an OpenAI OAuth access token when expired or within 5 minutes of expiry.
pub async fn ensure_fresh_voice_credential(
    provider: VoiceProvider,
    resolved: ResolvedVoiceCredential,
    store: &CredentialsStore,
    auth_base_override: Option<&str>,
) -> Result<ResolvedVoiceCredential> {
    if provider != VoiceProvider::Openai || resolved.auth_mode != AuthMode::Oauth {
        return Ok(resolved);
    }

    let now = now_unix_secs();
    let needs_refresh = resolved
        .expires_at
        .is_some_and(|exp| exp <= now.saturating_add(300));
    if !needs_refresh {
        return Ok(resolved);
    }

    let Some(ref refresh_tok) = resolved.refresh_token else {
        return Ok(resolved);
    };
    let cid = resolved
        .client_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_openai_client_id);

    let base = openai_auth_base_url(auth_base_override);
    let token_url = format!("{base}/oauth/token");
    let http = Client::builder().timeout(Duration::from_secs(12)).build()?;
    let resp = http
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_tok.as_str()),
            ("client_id", cid.as_str()),
        ])
        .send()
        .await
        .context("Не удалось обновить OAuth-токен OpenAI")?;

    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("OpenAI отклонил refresh_token ({status}) — войдите заново в Web UI");
    }

    let tok: OAuthTokenResp = resp
        .json()
        .await
        .context("Некорректный ответ OpenAI refresh_token")?;
    let new_refresh = tok.refresh_token.or_else(|| Some(refresh_tok.clone()));
    let new_exp = tok
        .expires_in
        .map(|s| now_unix_secs().saturating_add(s))
        .or(resolved.expires_at);
    let email = tok.id_token.as_deref().and_then(extract_email_from_jwt);

    if let Ok(mut file) = store.load() {
        if let Some(ref mut cur) = file.openai {
            cur.access_token = Some(tok.access_token.clone());
            cur.refresh_token = new_refresh.clone();
            cur.expires_at = new_exp;
            if email.is_some() {
                cur.account_label = email;
            }
            let _ = store.save(&file);
        }
    }

    Ok(ResolvedVoiceCredential {
        secret: tok.access_token,
        auth_mode: AuthMode::Oauth,
        source: "credentials.json",
        refresh_token: new_refresh,
        client_id: Some(cid),
        expires_at: new_exp,
    })
}

/// Validate a Gemini or OpenAI credential via real setup handshake before persisting or switching.
pub async fn verify_provider_credential(
    provider: VoiceProvider,
    secret: &str,
    model: &str,
    voice: &str,
    endpoint_override: Option<&str>,
) -> Result<()> {
    let cfg = LiveClientConfig {
        api_key: secret.trim().to_string(),
        model: model.to_string(),
        voice_name: voice.to_string(),
        system_instruction: "Credential verification handshake".to_string(),
        has_ha_tools: false,
        has_weather_tools: false,
        has_alarm_tools: false,
        has_spotify_tools: false,
        has_volume_tools: false,
        has_bluetooth_tools: false,
        has_stop_dialogue: false,
        endpoint: endpoint_override.map(str::to_string),
        cancellation_epoch: None,
        authorization: None,
        suppress_followup: None,
    };

    match provider {
        VoiceProvider::Gemini => GeminiLiveClient::verify_handshake(&cfg).await,
        VoiceProvider::Openai => OpenAiRealtimeClient::verify_handshake(&cfg).await,
    }
}

/// Extract `(code, optional state)` either from a pasted callback URL (`http://.../callback?code=...&state=...`)
/// or a raw authorization code string.
pub fn parse_pasted_oauth_callback(input: &str) -> Result<(String, Option<String>)> {
    let trimmed = input.trim();
    anyhow::ensure!(!trimmed.is_empty(), "Пустой callback URL или код");

    if let Some((_, query)) = trimmed.split_once('?') {
        let query_clean = query.split('#').next().unwrap_or(query);
        let params = parse_query_map(query_clean);
        if let Some(err) = params.get("error") {
            anyhow::bail!("Провайдер вернул ошибку авторизации: {err}");
        }
        let code = params
            .get("code")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .context("В ссылке отсутствует параметр code")?;
        let state = params
            .get("state")
            .filter(|s| !s.trim().is_empty())
            .cloned();
        return Ok((code, state));
    }

    Ok((trimmed.to_string(), None))
}

fn parse_query_map(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in q.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(urlencoding::decode(k), urlencoding::decode(v));
        }
    }
    map
}

fn extract_email_from_jwt(jwt: &str) -> Option<String> {
    use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
    use base64::Engine;

    let payload_b64 = jwt.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| URL_SAFE.decode(payload_b64))
        .ok()?;
    let val: Value = serde_json::from_slice(&decoded).ok()?;
    val.get("email")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
        out
    }

    pub fn decode(s: &str) -> String {
        let mut out = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(val) =
                    u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
                {
                    out.push(val);
                    i += 3;
                    continue;
                }
            } else if bytes[i] == b'+' {
                out.push(b' ');
                i += 1;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    #[test]
    fn test_parse_pasted_oauth_callback_url_and_raw_code() {
        let (code, state) = parse_pasted_oauth_callback(
            "http://localhost:1455/auth/callback?code=ac_12345&state=st_999#frag",
        )
        .unwrap();
        assert_eq!(code, "ac_12345");
        assert_eq!(state.as_deref(), Some("st_999"));

        let (raw, st2) = parse_pasted_oauth_callback("  bare_auth_code_xyz  ").unwrap();
        assert_eq!(raw, "bare_auth_code_xyz");
        assert!(st2.is_none());

        assert!(
            parse_pasted_oauth_callback("http://localhost:1455/callback?error=access_denied")
                .is_err()
        );
    }

    #[test]
    fn test_extract_email_from_jwt_payload() {
        let hdr = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"u1","email":"owner@example.com"}"#);
        let jwt = format!("{hdr}.{payload}.sig");
        assert_eq!(
            extract_email_from_jwt(&jwt).as_deref(),
            Some("owner@example.com")
        );
    }
}
