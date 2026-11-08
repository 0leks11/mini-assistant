//! Local Web Dashboard & Spotify OAuth PKCE Web Server.
//!
//! Serves the local web interface on home Wi-Fi (e.g. port 8080),
//! handling OAuth 2.0 PKCE login, status display, and playback test controls.

#![allow(dead_code)]

use super::crypto::pkce_code_challenge;
use super::web_api::SpotifyApiClient;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, RwLock};
use tracing::{debug, info, warn};

const SPOTIFY_SCOPES: &str =
    "user-read-playback-state user-modify-playback-state user-read-currently-playing streaming user-read-email user-read-private";

#[derive(Clone)]
struct PendingAuth {
    verifier: String,
    redirect_uri: String,
    created_at: Instant,
}

pub struct SpotifyWebServer {
    client_id: String,
    redirect_uri: String,
    spotify_client: Arc<RwLock<Option<SpotifyApiClient>>>,
    pending_auths: Arc<RwLock<HashMap<String, PendingAuth>>>,
    token_path: Option<std::path::PathBuf>,
}

impl SpotifyWebServer {
    pub fn new(
        client_id: String,
        redirect_uri: String,
        spotify_client: Arc<RwLock<Option<SpotifyApiClient>>>,
    ) -> Self {
        Self {
            client_id,
            redirect_uri,
            spotify_client,
            pending_auths: Arc::new(RwLock::new(HashMap::new())),
            token_path: None,
        }
    }

    pub fn with_token_path(mut self, path: std::path::PathBuf) -> Self {
        self.token_path = Some(path);
        self
    }

    pub async fn run(
        self: Arc<Self>,
        listener: TcpListener,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        info!(
            "Spotify web management server listening on {}",
            listener.local_addr()?
        );
        loop {
            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        debug!("Spotify web server received shutdown signal");
                        break;
                    }
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, peer)) => {
                            let server = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = server.handle_connection(stream).await {
                                    debug!(%peer, error = %e, "Web client connection closed");
                                }
                            });
                        }
                        Err(e) => {
                            warn!(error = %e, "Web listener accept error");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_connection(&self, mut stream: TcpStream) -> Result<()> {
        let mut buf = [0u8; 4096];
        let n = match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Ok(()), // Idle timeout, close connection cleanly
        };
        if n == 0 {
            return Ok(());
        }

        let req_str = String::from_utf8_lossy(&buf[..n]);
        let mut lines = req_str.lines();
        let first_line = lines.next().unwrap_or("");
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let full_path = parts.next().unwrap_or("/");

        let (path, query) = match full_path.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (full_path, None),
        };

        let mut host = None;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("host") {
                    host = Some(v.trim().to_string());
                    break;
                }
            }
        }

        let active_redirect_uri = match &host {
            Some(h) if !h.is_empty() => format!("http://{h}/spotify/callback"),
            _ => self.redirect_uri.clone(),
        };

        match (method, path) {
            ("GET", "/") | ("GET", "/setup") => {
                let html = self.render_dashboard(&active_redirect_uri).await;
                self.send_response(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    html.as_bytes(),
                )
                .await?;
            }
            ("GET", "/spotify/login") => {
                let (auth_url, state) = self.create_authorization_url(&active_redirect_uri).await;
                info!(%state, "Initiated Spotify PKCE authorization");
                let redirect_header = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {auth_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(redirect_header.as_bytes()).await?;
            }
            ("GET", "/spotify/callback") => {
                let params = parse_query(query.unwrap_or(""));
                let code = params.get("code").map(|s| s.as_str());
                let state = params.get("state").map(|s| s.as_str());

                match (code, state) {
                    (Some(c), Some(s)) => {
                        match self.exchange_code_for_token(c, s).await {
                            Ok(()) => {
                                let redirect = "HTTP/1.1 302 Found\r\nLocation: /?status=spotify_success\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                stream.write_all(redirect.as_bytes()).await?;
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed Spotify token exchange");
                                let err_html = format!("<h1>Ошибка авторизации</h1><p>{e}</p><p><a href='/'>Назад</a></p>");
                                self.send_response(
                                    &mut stream,
                                    "400 Bad Request",
                                    "text/html; charset=utf-8",
                                    err_html.as_bytes(),
                                )
                                .await?;
                            }
                        }
                    }
                    _ => {
                        self.send_response(
                            &mut stream,
                            "400 Bad Request",
                            "text/plain",
                            b"Missing code or state",
                        )
                        .await?;
                    }
                }
            }
            ("POST", "/api/spotify/pause") => {
                if let Some(client) = self.spotify_client.write().await.as_mut() {
                    let _ = client.pause(None).await;
                }
                self.send_json(&mut stream, &serde_json::json!({ "status": "ok" }))
                    .await?;
            }
            ("POST", "/api/spotify/play") => {
                if let Some(client) = self.spotify_client.write().await.as_mut() {
                    let _ = client.play(None, None, None).await;
                }
                self.send_json(&mut stream, &serde_json::json!({ "status": "ok" }))
                    .await?;
            }
            ("POST", "/api/spotify/next") => {
                if let Some(client) = self.spotify_client.write().await.as_mut() {
                    let _ = client.next(None).await;
                }
                self.send_json(&mut stream, &serde_json::json!({ "status": "ok" }))
                    .await?;
            }
            ("POST", "/api/spotify/previous") => {
                if let Some(client) = self.spotify_client.write().await.as_mut() {
                    let _ = client.previous(None).await;
                }
                self.send_json(&mut stream, &serde_json::json!({ "status": "ok" }))
                    .await?;
            }
            ("GET", "/api/spotify/status") => {
                let is_auth = self.spotify_client.read().await.is_some();
                self.send_json(&mut stream, &serde_json::json!({ "authorized": is_auth }))
                    .await?;
            }
            _ => {
                self.send_response(&mut stream, "404 Not Found", "text/plain", b"Not Found")
                    .await?;
            }
        }

        Ok(())
    }

    async fn create_authorization_url(&self, redirect_uri: &str) -> (String, String) {
        let state = uuid::Uuid::new_v4().to_string();
        let verifier = format!("{}-{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let challenge = pkce_code_challenge(&verifier);

        // Store verifier with 10 min TTL
        let mut auths = self.pending_auths.write().await;
        // Purge old
        auths.retain(|_, v| v.created_at.elapsed() < Duration::from_secs(600));
        auths.insert(
            state.clone(),
            PendingAuth {
                verifier,
                redirect_uri: redirect_uri.to_string(),
                created_at: Instant::now(),
            },
        );

        let url = format!(
            "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&state={}&scope={}&code_challenge_method=S256&code_challenge={}",
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(&state),
            urlencoding::encode(SPOTIFY_SCOPES),
            urlencoding::encode(&challenge),
        );

        (url, state)
    }

    async fn exchange_code_for_token(&self, code: &str, state: &str) -> Result<()> {
        let (verifier, redirect_uri) = {
            let mut auths = self.pending_auths.write().await;
            let pending = auths
                .remove(state)
                .context("Expired or invalid OAuth state")?;
            (pending.verifier, pending.redirect_uri)
        };

        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &self.client_id),
            ("code_verifier", &verifier),
        ];

        let http = reqwest::Client::new();
        let resp = http
            .post("https://accounts.spotify.com/api/token")
            .form(&params)
            .send()
            .await
            .context("Failed token exchange request")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Spotify token exchange rejected: {body}");
        }

        #[derive(serde::Deserialize)]
        struct TokenResp {
            access_token: String,
            refresh_token: Option<String>,
        }

        let token_data: TokenResp = resp.json().await.context("Failed to parse token json")?;
        let refresh_token = token_data.refresh_token.context("Missing refresh_token")?;

        info!("Spotify authorization successful! Storing refresh token");
        if let Some(ref path) = self.token_path {
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(e) = tokio::fs::write(path, refresh_token.as_bytes()).await {
                warn!(path = %path.display(), error = %e, "Failed to persist Spotify refresh token");
            } else {
                info!(path = %path.display(), "Persisted Spotify refresh token to file");
            }
        }
        let client = SpotifyApiClient::new(self.client_id.clone(), refresh_token);
        *self.spotify_client.write().await = Some(client);

        Ok(())
    }

    async fn render_dashboard(&self, active_redirect_uri: &str) -> String {
        let has_auth = self.spotify_client.read().await.is_some();
        let status_badge = if has_auth {
            r#"<span style="color: #2ea043; font-weight: bold;">● Подключен</span>"#
        } else {
            r#"<span style="color: #d29922; font-weight: bold;">○ Не подключен</span>"#
        };

        let action_btn = if has_auth {
            r#"<p style="color: #8b949e; margin-top: 10px;">Spotify активен. Вы можете переключать треки и ставить паузу голосом («Штучка, пауза», «Штучка, дальше»).</p>
               <div style="margin-top: 16px; display: flex; gap: 8px;">
                 <button onclick="fetch('/api/spotify/pause', {method:'POST'})" class="btn">Пауза</button>
                 <button onclick="fetch('/api/spotify/play', {method:'POST'})" class="btn">Играть</button>
                 <button onclick="fetch('/api/spotify/next', {method:'POST'})" class="btn">Дальше</button>
               </div>"#.to_string()
        } else if self.client_id.trim().is_empty() {
            r#"<p style="color: #8b949e; margin-top: 10px;">Для подключения Spotify укажите <code>client_id</code> в <code>config.toml</code> (секция <code>[spotify]</code>) или в переменной <code>SPOTIFY_CLIENT_ID</code>.</p>"#.to_string()
        } else {
            format!(
                r#"<a href="/spotify/login" class="btn-primary">Войти через Spotify (OAuth PKCE)</a>
                <div style="background: #21262d; border: 1px solid #30363d; border-radius: 8px; padding: 14px; margin-top: 20px; font-size: 13px; line-height: 1.5;">
                  <strong style="color: #f0883e;">⚙️ Настройка Spotify Developer Dashboard:</strong>
                  <p style="margin: 6px 0 4px 0;">Если при входе в Spotify возникает ошибка <em>«no matching config»</em>, добавьте Redirect URI в настройках вашего приложения на <a href="https://developer.spotify.com/dashboard" target="_blank" style="color: #58a6ff;">Spotify Dashboard</a>:</p>
                  <code style="background: #161b22; border: 1px solid #30363d; padding: 4px 8px; border-radius: 4px; display: block; word-break: break-all; color: #58a6ff; font-family: monospace; margin-top: 4px;">{active_redirect_uri}</code>
                </div>"#
            )
        };

        format!(
            r#"<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>GPT Mini — Управление</title>
<style>
:root {{ --bg: #0d1117; --card: #161b22; --border: #30363d; --text: #c9d1d9; --accent: #1db954; }}
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: var(--bg); color: var(--text); padding: 24px; display: flex; justify-content: center; }}
.card {{ background: var(--card); border: 1px solid var(--border); border-radius: 12px; padding: 24px; width: 100%; max-width: 480px; box-shadow: 0 8px 24px rgba(0,0,0,0.5); }}
h1 {{ font-size: 20px; color: #fff; margin-bottom: 12px; }}
.badge-row {{ margin: 16px 0; font-size: 15px; }}
.btn {{ background: #21262d; border: 1px solid var(--border); color: #fff; padding: 8px 16px; border-radius: 6px; cursor: pointer; }}
.btn-primary {{ display: inline-block; background: var(--accent); color: #fff; text-decoration: none; font-weight: 600; padding: 12px 20px; border-radius: 8px; margin-top: 12px; }}
</style>
</head>
<body>
<div class="card">
  <h1>Управление Станцией Mini</h1>
  <div class="badge-row">Статус Spotify: {status_badge}</div>
  {action_btn}
</div>
</body>
</html>"#
        )
    }

    async fn send_response(
        &self,
        stream: &mut TcpStream,
        status: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<()> {
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(body).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn send_json(&self, stream: &mut TcpStream, val: &serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(val)?;
        self.send_response(stream, "200 OK", "application/json", &bytes)
            .await
    }
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in q.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(urlencoding::decode(k), urlencoding::decode(v));
        }
    }
    map
}

mod urlencoding {
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

pub fn load_persisted_token(path: &std::path::Path) -> Option<String> {
    if let Ok(content) = std::fs::read_to_string(path) {
        let trimmed = content.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_web_server_dashboard_and_login_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let spotify_client = Arc::new(RwLock::new(None));
        let server = Arc::new(SpotifyWebServer::new(
            "test-client-id".into(),
            "http://127.0.0.1:8080/spotify/callback".into(),
            spotify_client,
        ));

        let srv_handle = tokio::spawn(server.run(listener, shutdown_rx));

        // 1. GET /
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("Статус Spotify:"));
        assert!(resp.contains("Не подключен"));

        // 2. GET /spotify/login
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /spotify/login HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 302 Found"));
        assert!(resp.contains("Location: https://accounts.spotify.com/authorize?"));
        assert!(resp.contains("code_challenge="));
        assert!(resp.contains("code_challenge_method=S256"));

        let _ = shutdown_tx.send(true);
        let _ = srv_handle.await;
    }

    #[tokio::test]
    async fn test_callback_rejects_missing_or_tampered_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let spotify_client = Arc::new(RwLock::new(None));
        let server = Arc::new(SpotifyWebServer::new(
            "test-client-id".into(),
            "http://127.0.0.1:8080/spotify/callback".into(),
            spotify_client,
        ));

        let srv_handle = tokio::spawn(server.run(listener, shutdown_rx));

        // Request callback with bogus state
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /spotify/callback?code=mock_code&state=bogus_state HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();

        assert!(resp.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(resp.contains("Expired or invalid OAuth state"));

        let _ = shutdown_tx.send(true);
        let _ = srv_handle.await;
    }
}
