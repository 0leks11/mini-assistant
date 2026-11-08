//! Spotify Web API Client in pure Rust using reqwest.
//!
//! Provides player controls (play, pause, next, volume) and search
//! via official Spotify Web API endpoints with OAuth 2.0 PKCE token refresh.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tracing::{debug, info};

const DEFAULT_ACCOUNTS_URL: &str = "https://accounts.spotify.com/api/token";
const DEFAULT_API_BASE_URL: &str = "https://api.spotify.com";

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: Option<String>,
    pub name: String,
    pub is_active: bool,
    pub volume_percent: Option<u32>,
}

/// Selects the best target device for playback:
/// 1. Prefer local GPT Mini ("GPT Mini" or containing "mini").
/// 2. Prefer an already active device.
/// 3. Fallback to any available device with an ID.
pub fn select_target_device(devices: &[DeviceInfo]) -> Option<&DeviceInfo> {
    devices
        .iter()
        .find(|d| {
            d.id.is_some()
                && (d.name.eq_ignore_ascii_case("GPT Mini")
                    || d.name.to_lowercase().contains("gpt-mini")
                    || d.name.to_lowercase().contains("mini"))
        })
        .or_else(|| devices.iter().find(|d| d.is_active && d.id.is_some()))
        .or_else(|| devices.iter().find(|d| d.id.is_some()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackItem {
    pub id: Option<String>,
    pub name: String,
    pub uri: String,
    #[serde(default)]
    pub artists: Vec<ArtistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtistItem {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub device: Option<DeviceInfo>,
    pub item: Option<TrackItem>,
    pub progress_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchItem {
    pub name: String,
    pub uri: String,
    #[serde(default)]
    pub artists: Vec<ArtistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResults {
    pub tracks: Vec<SearchItem>,
    pub playlists: Vec<SearchItem>,
}

#[derive(Debug, Clone, Serialize)]
struct PlayBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    context_uri: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uris: Option<&'a [&'a str]>,
}

pub struct SpotifyApiClient {
    client_id: String,
    refresh_token: String,
    token_url: String,
    api_base_url: String,
    access_token: Option<String>,
    expires_at: Option<Instant>,
    http: reqwest::Client,
}

impl SpotifyApiClient {
    pub fn new(client_id: String, refresh_token: String) -> Self {
        Self {
            client_id,
            refresh_token,
            token_url: DEFAULT_ACCOUNTS_URL.to_string(),
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            access_token: None,
            expires_at: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    #[doc(hidden)]
    pub fn with_custom_endpoints(
        client_id: String,
        refresh_token: String,
        token_url: String,
        api_base_url: String,
    ) -> Self {
        Self {
            client_id,
            refresh_token,
            token_url,
            api_base_url,
            access_token: None,
            expires_at: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub async fn ensure_valid_token(&mut self) -> Result<String> {
        let now = Instant::now();
        if let (Some(token), Some(exp)) = (&self.access_token, self.expires_at) {
            // Keep token if at least 30 seconds of life remain
            if exp > now + Duration::from_secs(30) {
                return Ok(token.clone());
            }
        }
        self.refresh_token_request().await
    }

    async fn refresh_token_request(&mut self) -> Result<String> {
        debug!(url = %self.token_url, "Refreshing Spotify access token");
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", &self.refresh_token),
            ("client_id", &self.client_id),
        ];

        let resp = self
            .http
            .post(&self.token_url)
            .form(&params)
            .send()
            .await
            .context("Spotify token refresh request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            bail!("Spotify token refresh rejected ({status}): {err_body}");
        }

        let token_resp: TokenResponse = resp
            .json()
            .await
            .context("Cannot parse Spotify token response")?;

        let access_token = token_resp.access_token;
        self.access_token = Some(access_token.clone());
        self.expires_at = Some(Instant::now() + Duration::from_secs(token_resp.expires_in));

        // If Spotify rotated the refresh token, store the new one
        if let Some(new_rt) = token_resp.refresh_token {
            if !new_rt.is_empty() && new_rt != self.refresh_token {
                info!("Spotify refresh token was rotated by accounts service");
                self.refresh_token = new_rt;
            }
        }

        Ok(access_token)
    }

    pub async fn pause(&mut self, device_id: Option<&str>) -> Result<()> {
        let mut url = format!("{}/v1/me/player/pause", self.api_base_url);
        if let Some(dev) = device_id {
            url.push_str(&format!("?device_id={dev}"));
        }
        let first = self
            .send_authenticated_request(reqwest::Method::PUT, &url, None::<&()>)
            .await;
        if let Err(e) = first {
            let err_str = e.to_string();
            if device_id.is_none()
                && (err_str.contains("404")
                    || err_str.to_lowercase().contains("device")
                    || err_str.to_lowercase().contains("active"))
            {
                if let Ok(devices) = self.get_devices().await {
                    if let Some(target) = select_target_device(&devices) {
                        if let Some(ref target_id) = target.id {
                            let retry_url = format!(
                                "{}/v1/me/player/pause?device_id={target_id}",
                                self.api_base_url
                            );
                            return self
                                .send_authenticated_request(
                                    reqwest::Method::PUT,
                                    &retry_url,
                                    None::<&()>,
                                )
                                .await
                                .context("Spotify pause on discovered device failed");
                        }
                    }
                }
            }
            return Err(e);
        }
        Ok(())
    }

    pub async fn play(
        &mut self,
        context_uri: Option<&str>,
        uris: Option<&[&str]>,
        device_id: Option<&str>,
    ) -> Result<()> {
        let mut url = format!("{}/v1/me/player/play", self.api_base_url);
        if let Some(dev) = device_id {
            url.push_str(&format!("?device_id={dev}"));
        }

        let has_body = context_uri.is_some() || uris.is_some();
        let body = if has_body {
            Some(PlayBody { context_uri, uris })
        } else {
            None
        };

        let first = self
            .send_authenticated_request(reqwest::Method::PUT, &url, body.as_ref())
            .await;

        if let Err(e) = first {
            let err_str = e.to_string();
            if device_id.is_none()
                && (err_str.contains("404")
                    || err_str.to_lowercase().contains("device")
                    || err_str.to_lowercase().contains("active"))
            {
                debug!("Play attempt returned {err_str}; querying available devices");
                if let Ok(devices) = self.get_devices().await {
                    if let Some(target) = select_target_device(&devices) {
                        if let Some(ref target_id) = target.id {
                            let retry_url = format!(
                                "{}/v1/me/player/play?device_id={target_id}",
                                self.api_base_url
                            );
                            info!(target_name = %target.name, "Retrying Spotify play on discovered device");
                            return self
                                .send_authenticated_request(
                                    reqwest::Method::PUT,
                                    &retry_url,
                                    body.as_ref(),
                                )
                                .await
                                .context("Spotify play on discovered device failed");
                        }
                    }
                    bail!("Нет доступных устройств Spotify. Откройте приложение Spotify на телефоне или компьютере.");
                }
            }
            return Err(e);
        }
        Ok(())
    }

    pub async fn resume(&mut self, device_id: Option<&str>) -> Result<()> {
        self.play(None, None, device_id).await
    }

    pub async fn search_and_play(&mut self, query: &str) -> Result<String> {
        let results = self.search(query, "track,playlist", 1).await?;
        if let Some(track) = results.tracks.first() {
            let uris = [track.uri.as_str()];
            self.play(None, Some(&uris), None).await?;
            let artist = track.artists.first().map(|a| a.name.as_str()).unwrap_or("");
            return Ok(if artist.is_empty() {
                format!("Включаю {}", track.name)
            } else {
                format!("Включаю {} — {}", track.name, artist)
            });
        }
        if let Some(playlist) = results.playlists.first() {
            self.play(Some(&playlist.uri), None, None).await?;
            return Ok(format!("Включаю плейлист {}", playlist.name));
        }
        bail!("Ничего не найдено в Spotify по запросу: {query}")
    }

    pub async fn next(&mut self, device_id: Option<&str>) -> Result<()> {
        let mut url = format!("{}/v1/me/player/next", self.api_base_url);
        if let Some(dev) = device_id {
            url.push_str(&format!("?device_id={dev}"));
        }
        let first = self
            .send_authenticated_request(reqwest::Method::POST, &url, None::<&()>)
            .await;
        if let Err(e) = first {
            let err_str = e.to_string();
            if device_id.is_none()
                && (err_str.contains("404")
                    || err_str.to_lowercase().contains("device")
                    || err_str.to_lowercase().contains("active"))
            {
                if let Ok(devices) = self.get_devices().await {
                    if let Some(target) = select_target_device(&devices) {
                        if let Some(ref target_id) = target.id {
                            let retry_url = format!(
                                "{}/v1/me/player/next?device_id={target_id}",
                                self.api_base_url
                            );
                            return self
                                .send_authenticated_request(
                                    reqwest::Method::POST,
                                    &retry_url,
                                    None::<&()>,
                                )
                                .await
                                .context("Spotify next on discovered device failed");
                        }
                    }
                }
            }
            return Err(e);
        }
        Ok(())
    }

    pub async fn previous(&mut self, device_id: Option<&str>) -> Result<()> {
        let mut url = format!("{}/v1/me/player/previous", self.api_base_url);
        if let Some(dev) = device_id {
            url.push_str(&format!("?device_id={dev}"));
        }
        let first = self
            .send_authenticated_request(reqwest::Method::POST, &url, None::<&()>)
            .await;
        if let Err(e) = first {
            let err_str = e.to_string();
            if device_id.is_none()
                && (err_str.contains("404")
                    || err_str.to_lowercase().contains("device")
                    || err_str.to_lowercase().contains("active"))
            {
                if let Ok(devices) = self.get_devices().await {
                    if let Some(target) = select_target_device(&devices) {
                        if let Some(ref target_id) = target.id {
                            let retry_url = format!(
                                "{}/v1/me/player/previous?device_id={target_id}",
                                self.api_base_url
                            );
                            return self
                                .send_authenticated_request(
                                    reqwest::Method::POST,
                                    &retry_url,
                                    None::<&()>,
                                )
                                .await
                                .context("Spotify previous on discovered device failed");
                        }
                    }
                }
            }
            return Err(e);
        }
        Ok(())
    }

    pub async fn set_volume(&mut self, volume_percent: u8, device_id: Option<&str>) -> Result<()> {
        let clamped = volume_percent.min(100);
        let mut url = format!(
            "{}/v1/me/player/volume?volume_percent={clamped}",
            self.api_base_url
        );
        if let Some(dev) = device_id {
            url.push_str(&format!("&device_id={dev}"));
        }
        let first = self
            .send_authenticated_request(reqwest::Method::PUT, &url, None::<&()>)
            .await;
        if let Err(e) = first {
            let err_str = e.to_string();
            if device_id.is_none()
                && (err_str.contains("404")
                    || err_str.to_lowercase().contains("device")
                    || err_str.to_lowercase().contains("active"))
            {
                if let Ok(devices) = self.get_devices().await {
                    if let Some(target) = select_target_device(&devices) {
                        if let Some(ref target_id) = target.id {
                            let retry_url = format!(
                                "{}/v1/me/player/volume?volume_percent={clamped}&device_id={target_id}",
                                self.api_base_url
                            );
                            return self
                                .send_authenticated_request(
                                    reqwest::Method::PUT,
                                    &retry_url,
                                    None::<&()>,
                                )
                                .await
                                .context("Spotify volume on discovered device failed");
                        }
                    }
                }
            }
            return Err(e);
        }
        Ok(())
    }

    pub async fn get_devices(&mut self) -> Result<Vec<DeviceInfo>> {
        let url = format!("{}/v1/me/player/devices", self.api_base_url);
        let token = self.ensure_valid_token().await?;

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Spotify get_devices request failed")?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let new_token = self.refresh_token_request().await?;
            let retry = self
                .http
                .get(&url)
                .bearer_auth(&new_token)
                .send()
                .await
                .context("Spotify get_devices retry failed")?;
            if !retry.status().is_success() {
                bail!("Spotify get_devices error ({})", retry.status());
            }
            let val: serde_json::Value = retry.json().await?;
            return Ok(parse_devices_json(&val));
        }

        if !resp.status().is_success() {
            bail!("Spotify get_devices error ({})", resp.status());
        }

        let val: serde_json::Value = resp.json().await?;
        Ok(parse_devices_json(&val))
    }

    pub async fn get_playback_state(&mut self) -> Result<Option<PlaybackState>> {
        let url = format!("{}/v1/me/player", self.api_base_url);
        let token = self.ensure_valid_token().await?;

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Spotify get_playback_state request failed")?;

        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Token expired, force refresh and retry once
            let new_token = self.refresh_token_request().await?;
            let retry = self
                .http
                .get(&url)
                .bearer_auth(&new_token)
                .send()
                .await
                .context("Spotify get_playback_state retry failed")?;

            if retry.status() == reqwest::StatusCode::NO_CONTENT {
                return Ok(None);
            }
            if !retry.status().is_success() {
                bail!("Spotify get_playback_state error ({})", retry.status());
            }
            let state: PlaybackState = retry.json().await?;
            return Ok(Some(state));
        }

        if !resp.status().is_success() {
            bail!("Spotify get_playback_state error ({})", resp.status());
        }

        let state: PlaybackState = resp.json().await?;
        Ok(Some(state))
    }

    pub async fn search(
        &mut self,
        query: &str,
        search_type: &str,
        limit: u8,
    ) -> Result<SearchResults> {
        let url = format!("{}/v1/search", self.api_base_url);
        let token = self.ensure_valid_token().await?;

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .query(&[
                ("q", query),
                ("type", search_type),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .context("Spotify search request failed")?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let new_token = self.refresh_token_request().await?;
            let retry = self
                .http
                .get(&url)
                .bearer_auth(&new_token)
                .query(&[
                    ("q", query),
                    ("type", search_type),
                    ("limit", &limit.to_string()),
                ])
                .send()
                .await
                .context("Spotify search retry failed")?;
            if !retry.status().is_success() {
                bail!("Spotify search error ({})", retry.status());
            }
            return parse_search_json(retry.json().await?);
        }

        if !status.is_success() {
            bail!("Spotify search error ({status})");
        }

        parse_search_json(resp.json().await?)
    }

    async fn send_authenticated_request<T: Serialize>(
        &mut self,
        method: reqwest::Method,
        url: &str,
        json_body: Option<&T>,
    ) -> Result<()> {
        let token = self.ensure_valid_token().await?;
        let mut req = self.http.request(method.clone(), url).bearer_auth(&token);
        if let Some(body) = json_body {
            req = req.json(body);
        } else if method == reqwest::Method::POST || method == reqwest::Method::PUT {
            req = req.header(reqwest::header::CONTENT_LENGTH, 0);
        }

        let resp = req.send().await.context("Spotify API request failed")?;
        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            debug!("Got 401 from Spotify API, refreshing token and retrying");
            let new_token = self.refresh_token_request().await?;
            let mut retry_req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(&new_token);
            if let Some(body) = json_body {
                retry_req = retry_req.json(body);
            } else if method == reqwest::Method::POST || method == reqwest::Method::PUT {
                retry_req = retry_req.header(reqwest::header::CONTENT_LENGTH, 0);
            }
            let retry_resp = retry_req.send().await.context("Spotify API retry failed")?;
            let retry_status = retry_resp.status();
            if !retry_status.is_success() && retry_status != reqwest::StatusCode::NO_CONTENT {
                let err = retry_resp.text().await.unwrap_or_default();
                bail!("Spotify API call failed on retry ({retry_status}): {err}");
            }
            return Ok(());
        }

        if !status.is_success() && status != reqwest::StatusCode::NO_CONTENT {
            let err = resp.text().await.unwrap_or_default();
            bail!("Spotify API call failed ({status}): {err}");
        }

        Ok(())
    }
}

fn parse_search_json(val: serde_json::Value) -> Result<SearchResults> {
    let mut tracks = Vec::new();
    let mut playlists = Vec::new();

    if let Some(items) = val["tracks"]["items"].as_array() {
        for it in items {
            if let (Some(name), Some(uri)) = (it["name"].as_str(), it["uri"].as_str()) {
                let mut artists = Vec::new();
                if let Some(arr) = it["artists"].as_array() {
                    for a in arr {
                        if let Some(aname) = a["name"].as_str() {
                            artists.push(ArtistItem {
                                name: aname.to_string(),
                            });
                        }
                    }
                }
                tracks.push(SearchItem {
                    name: name.to_string(),
                    uri: uri.to_string(),
                    artists,
                });
            }
        }
    }

    if let Some(items) = val["playlists"]["items"].as_array() {
        for it in items {
            if let (Some(name), Some(uri)) = (it["name"].as_str(), it["uri"].as_str()) {
                playlists.push(SearchItem {
                    name: name.to_string(),
                    uri: uri.to_string(),
                    artists: vec![],
                });
            }
        }
    }

    Ok(SearchResults { tracks, playlists })
}

fn parse_devices_json(val: &serde_json::Value) -> Vec<DeviceInfo> {
    let mut list = Vec::new();
    if let Some(devices) = val["devices"].as_array() {
        for d in devices {
            if let Some(name) = d["name"].as_str() {
                list.push(DeviceInfo {
                    id: d["id"].as_str().map(|s| s.to_string()),
                    name: name.to_string(),
                    is_active: d["is_active"].as_bool().unwrap_or(false),
                    volume_percent: d["volume_percent"].as_u64().map(|v| v as u32),
                });
            }
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_spotify_token_refresh_and_rotation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("grant_type=refresh_token"));
            assert!(req.contains("refresh_token=old-refresh-token"));
            assert!(req.contains("client_id=test-client-id"));

            let json_body = serde_json::json!({
                "access_token": "new-access-token-123",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "rotated-refresh-token-456"
            });
            let body_bytes = serde_json::to_vec(&json_body).unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body_bytes.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            stream.write_all(&body_bytes).await.unwrap();
        });

        let mut client = SpotifyApiClient::with_custom_endpoints(
            "test-client-id".to_string(),
            "old-refresh-token".to_string(),
            format!("http://{addr}/token"),
            format!("http://{addr}"),
        );

        let token = client.ensure_valid_token().await.expect("Refresh ok");
        assert_eq!(token, "new-access-token-123");
        assert_eq!(client.refresh_token(), "rotated-refresh-token-456");

        let _ = server.await;
    }

    #[tokio::test]
    async fn test_spotify_pause_and_next_player_commands() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let pause_called = Arc::new(AtomicUsize::new(0));
        let next_called = Arc::new(AtomicUsize::new(0));

        let p_clone = pause_called.clone();
        let n_clone = next_called.clone();

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);

                assert!(req
                    .to_lowercase()
                    .contains("authorization: bearer mock-token"));

                if req.starts_with("PUT /v1/me/player/pause") {
                    p_clone.fetch_add(1, Ordering::SeqCst);
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                } else if req.starts_with("POST /v1/me/player/next") {
                    n_clone.fetch_add(1, Ordering::SeqCst);
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                }
            }
        });

        let mut client = SpotifyApiClient::with_custom_endpoints(
            "client-id".into(),
            "token".into(),
            format!("http://{addr}/token"),
            format!("http://{addr}"),
        );
        client.access_token = Some("mock-token".into());
        client.expires_at = Some(Instant::now() + Duration::from_secs(3600));

        client.pause(None).await.expect("Pause ok");
        client.next(None).await.expect("Next ok");

        assert_eq!(pause_called.load(Ordering::SeqCst), 1);
        assert_eq!(next_called.load(Ordering::SeqCst), 1);

        let _ = server.await;
    }

    #[tokio::test]
    async fn test_spotify_401_triggers_automatic_refresh_and_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            // 1. Initial play call -> responds 401 Unauthorized
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await.unwrap();
                stream
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }

            // 2. Refresh token call -> responds 200 with new token
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await.unwrap();
                let body = serde_json::to_vec(&serde_json::json!({
                    "access_token": "fresh-token-999",
                    "token_type": "Bearer",
                    "expires_in": 3600
                }))
                .unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }

            // 3. Retry play call with fresh token -> responds 204 No Content
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                assert!(req
                    .to_lowercase()
                    .contains("authorization: bearer fresh-token-999"));
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }
        });

        let mut client = SpotifyApiClient::with_custom_endpoints(
            "client-id".into(),
            "refresh-token".into(),
            format!("http://{addr}/token"),
            format!("http://{addr}"),
        );
        client.access_token = Some("stale-token".into());
        client.expires_at = Some(Instant::now() + Duration::from_secs(3600));

        // Call play -> should transparently recover from 401
        client.play(None, None, None).await.expect("Play recovered");

        let _ = server.await;
    }

    #[tokio::test]
    async fn test_empty_post_and_put_send_content_length_zero() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(
                req.to_lowercase().contains("content-length: 0"),
                "Expected Content-Length: 0 header in request, got:\n{req}"
            );
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });

        let mut client = SpotifyApiClient::with_custom_endpoints(
            "client-id".into(),
            "refresh-token".into(),
            format!("http://{addr}/token"),
            format!("http://{addr}"),
        );
        client.access_token = Some("valid-token".into());
        client.expires_at = Some(Instant::now() + Duration::from_secs(3600));

        client.resume(None).await.expect("Resume succeeded");
        let _ = server.await;
    }

    #[test]
    fn test_select_target_device_prioritizes_mini_over_laptop() {
        let macbook = DeviceInfo {
            id: Some("macbook-id".into()),
            name: "MacBook Pro (4)".into(),
            is_active: true,
            volume_percent: Some(80),
        };
        let mini = DeviceInfo {
            id: Some("mini-id".into()),
            name: "GPT Mini".into(),
            is_active: false,
            volume_percent: Some(50),
        };
        let phone = DeviceInfo {
            id: Some("phone-id".into()),
            name: "Nothing Phone (1)".into(),
            is_active: false,
            volume_percent: Some(60),
        };

        // Even if MacBook is active, GPT Mini must be prioritized
        let devices = vec![macbook.clone(), mini.clone(), phone.clone()];
        let target = select_target_device(&devices).expect("found target");
        assert_eq!(target.id.as_deref(), Some("mini-id"));

        // If Mini is absent, active device is selected
        let no_mini = vec![phone.clone(), macbook.clone()];
        let target2 = select_target_device(&no_mini).expect("found target");
        assert_eq!(target2.id.as_deref(), Some("macbook-id"));
    }
}
