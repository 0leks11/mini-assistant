//! Control Center (Web UI & Telegram Mini App) Server for GPT Mini.
//!
//! Provides a single-binary, zero-dependency async dashboard on port 8080:
//! - Real-time metrics (RAM, CPU, Uptime, Wi-Fi)
//! - Interactive Master Volume control (0-10) with acoustic ducking sync
//! - Instant Voice selection (Callirrhoe, Puck, Autonoe, Charon, Kore, Fenrir, Aoede)
//! - Hardware SN3236 LED ring color palette
//! - Spotify Connect controls & PKCE OAuth flow
//! - One-click Assistant restart & Linux SoC reboot

use crate::audio::volume;
use crate::spotify::crypto::pkce_code_challenge;
use crate::spotify::web_api::SpotifyApiClient;
use crate::spotify::{MusicDucker, PlaybackState, SoloistClient};
use crate::wakeword::enrollment::{decode_wav_pcm16, EnrollmentSession, HardwareRecorder};
use anyhow::{Context, Result};
use rustpotter::{WakewordLoad, WakewordRef};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, RwLock};
use tracing::{debug, info, warn};

const DASHBOARD_HTML: &str = include_str!("dashboard.html");
const SPOTIFY_SCOPES: &str =
    "user-read-playback-state user-modify-playback-state user-read-currently-playing streaming user-read-email user-read-private";

#[derive(Clone)]
struct PendingAuth {
    verifier: String,
    redirect_uri: String,
    created_at: Instant,
}

pub struct ControlCenterServer {
    config_path: Option<PathBuf>,
    spotify_client: Arc<RwLock<Option<SpotifyApiClient>>>,
    soloist_client: Option<Arc<SoloistClient>>,
    ducker: Arc<MusicDucker>,
    token_path: Option<PathBuf>,
    client_id: String,
    redirect_uri: String,
    pending_auths: Arc<RwLock<HashMap<String, PendingAuth>>>,
    start_time: Instant,
    current_led_color: Arc<RwLock<String>>,
    wakeword_session: Arc<RwLock<Option<EnrollmentSession>>>,
    hardware_recorder: Option<HardwareRecorder>,
    wakeword_reload_tx: Option<mpsc::UnboundedSender<PathBuf>>,
    wakeword_model_path: PathBuf,
}

impl ControlCenterServer {
    pub fn new(
        config_path: Option<PathBuf>,
        client_id: String,
        redirect_uri: String,
        spotify_client: Arc<RwLock<Option<SpotifyApiClient>>>,
        soloist_client: Option<Arc<SoloistClient>>,
        ducker: Arc<MusicDucker>,
    ) -> Self {
        Self {
            config_path,
            spotify_client,
            soloist_client,
            ducker,
            token_path: None,
            client_id,
            redirect_uri,
            pending_auths: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
            current_led_color: Arc::new(RwLock::new("azure".to_string())),
            wakeword_session: Arc::new(RwLock::new(None)),
            hardware_recorder: None,
            wakeword_reload_tx: None,
            wakeword_model_path: PathBuf::from("/data/opt/mini-assistant/keyword.rpw"),
        }
    }

    pub fn with_token_path(mut self, path: PathBuf) -> Self {
        self.token_path = Some(path);
        self
    }

    pub fn with_wakeword_support(
        mut self,
        recorder: Option<HardwareRecorder>,
        reload_tx: Option<mpsc::UnboundedSender<PathBuf>>,
        model_path: PathBuf,
    ) -> Self {
        self.hardware_recorder = recorder;
        self.wakeword_reload_tx = reload_tx;
        self.wakeword_model_path = model_path;
        self
    }

    pub async fn run(
        self: Arc<Self>,
        listener: TcpListener,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        info!(
            "GPT Mini Control Center listening on http://{}",
            listener.local_addr()?
        );
        loop {
            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        debug!("Control Center web server received shutdown signal");
                        break;
                    }
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, peer)) => {
                            let server = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = server.handle_connection(stream).await {
                                    debug!(%peer, error = %e, "Control Center connection closed");
                                }
                            });
                        }
                        Err(e) => {
                            warn!(error = %e, "Control Center accept error");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_connection(&self, mut stream: TcpStream) -> Result<()> {
        let mut buf = [0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Ok(()),
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

        // Extract body if present (with multi-chunk reading for uploads up to 512KB)
        let header_end = req_str
            .find("\r\n\r\n")
            .map(|p| (p, 4))
            .or_else(|| req_str.find("\n\n").map(|p| (p, 2)));

        let mut content_length = 0usize;
        let mut host = None;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let k_trim = k.trim();
                if k_trim.eq_ignore_ascii_case("content-length") {
                    if let Ok(len) = v.trim().parse::<usize>() {
                        content_length = len.min(512 * 1024);
                    }
                } else if k_trim.eq_ignore_ascii_case("host") {
                    host = Some(v.trim().to_string());
                }
            }
        }

        let body_owned: String;
        let body: &str = if let Some((pos, delim_len)) = header_end {
            let body_start = pos + delim_len;
            let already_read = n.saturating_sub(body_start);
            if content_length > already_read && content_length <= 512 * 1024 {
                let mut full_body = Vec::with_capacity(content_length);
                full_body.extend_from_slice(&buf[body_start..n]);
                let mut remaining = content_length - already_read;
                let mut chunk_buf = [0u8; 8192];
                while remaining > 0 {
                    let to_read = remaining.min(chunk_buf.len());
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        stream.read(&mut chunk_buf[..to_read]),
                    )
                    .await
                    {
                        Ok(Ok(read_bytes)) if read_bytes > 0 => {
                            full_body.extend_from_slice(&chunk_buf[..read_bytes]);
                            remaining -= read_bytes;
                        }
                        _ => break,
                    }
                }
                body_owned = String::from_utf8_lossy(&full_body).to_string();
                &body_owned
            } else {
                &req_str[body_start..n]
            }
        } else {
            ""
        };

        let active_redirect_uri = match &host {
            Some(h) if !h.is_empty() => format!("http://{h}/spotify/callback"),
            _ => self.redirect_uri.clone(),
        };

        match (method, path) {
            // Dashboard UI
            ("GET", "/") | ("GET", "/index.html") | ("GET", "/setup") => {
                self.send_response(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    DASHBOARD_HTML.as_bytes(),
                )
                .await?;
            }

            // REST API v1
            ("GET", "/api/v1/status") => {
                self.handle_api_status(&mut stream).await?;
            }
            ("POST", "/api/v1/volume") => {
                self.handle_api_volume(&mut stream, body).await?;
            }
            ("POST", "/api/v1/voice") => {
                self.handle_api_voice(&mut stream, body).await?;
            }
            ("POST", "/api/v1/led") => {
                self.handle_api_led(&mut stream, body).await?;
            }
            ("POST", "/api/v1/system/restart") => {
                self.handle_api_system_restart(&mut stream).await?;
            }
            ("POST", "/api/v1/system/reboot") => {
                self.handle_api_system_reboot(&mut stream).await?;
            }

            // Wake-Word Custom Enrollment REST API
            ("POST", "/api/v1/wakeword/session/start") => {
                self.handle_api_wakeword_session_start(&mut stream, body)
                    .await?;
            }
            ("GET", "/api/v1/wakeword/session/status") => {
                self.handle_api_wakeword_session_status(&mut stream).await?;
            }
            ("POST", "/api/v1/wakeword/sample/record_hw") => {
                self.handle_api_wakeword_record_hw(&mut stream).await?;
            }
            ("POST", "/api/v1/wakeword/sample/upload") => {
                self.handle_api_wakeword_upload_sample(&mut stream, body)
                    .await?;
            }
            ("POST", "/api/v1/wakeword/sample/remove") => {
                self.handle_api_wakeword_remove_sample(&mut stream, body)
                    .await?;
            }
            ("POST", "/api/v1/wakeword/compile") => {
                self.handle_api_wakeword_compile(&mut stream).await?;
            }

            // Spotify OAuth PKCE Flow
            ("GET", "/spotify/login") => {
                let (auth_url, state) = self.create_authorization_url(&active_redirect_uri).await;
                info!(%state, "Initiated Spotify PKCE authorization");
                let redirect_header = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {auth_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(redirect_header.as_bytes()).await?;
            }
            ("GET", "/spotify/callback") => {
                self.handle_spotify_callback(&mut stream, query).await?;
            }

            // Spotify Playback API (legacy & backward-compatible)
            ("POST", "/api/spotify/pause") => {
                if let Some(ref soloist) = self.soloist_client {
                    let _ = soloist.pause().await;
                } else if let Some(client) = self.spotify_client.write().await.as_mut() {
                    let _ = client.pause(None).await;
                }
                self.send_json(&mut stream, &serde_json::json!({ "status": "ok" }))
                    .await?;
            }
            ("POST", "/api/spotify/play") => {
                if let Some(ref soloist) = self.soloist_client {
                    let _ = soloist.resume().await;
                } else if let Some(client) = self.spotify_client.write().await.as_mut() {
                    let _ = client.play(None, None, None).await;
                }
                self.send_json(&mut stream, &serde_json::json!({ "status": "ok" }))
                    .await?;
            }
            ("POST", "/api/spotify/next") => {
                if let Some(ref soloist) = self.soloist_client {
                    let _ = soloist.next().await;
                } else if let Some(client) = self.spotify_client.write().await.as_mut() {
                    let _ = client.next(None).await;
                }
                self.send_json(&mut stream, &serde_json::json!({ "status": "ok" }))
                    .await?;
            }
            ("POST", "/api/spotify/previous") => {
                if let Some(ref soloist) = self.soloist_client {
                    let _ = soloist.previous().await;
                } else if let Some(client) = self.spotify_client.write().await.as_mut() {
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

    async fn handle_api_status(&self, stream: &mut TcpStream) -> Result<()> {
        let metrics = read_system_metrics(self.start_time);
        let cur_vol = volume::current_level();
        let cur_voice = self.read_current_voice();
        let cur_led = self.current_led_color.read().await.clone();

        // Query Spotify Soloist track
        let mut spotify_info = serde_json::json!({
            "authorized": self.spotify_client.read().await.is_some(),
            "is_playing": false,
            "track": null,
            "artist": null,
        });

        if let Some(ref soloist) = self.soloist_client {
            if let Ok(st) = soloist.get_status().await {
                let is_playing = st.state == PlaybackState::Playing;
                let track_name = st.track.as_ref().map(|t| t.name.clone());
                let artist_name = st.track.as_ref().map(|t| t.artist.clone());
                spotify_info = serde_json::json!({
                    "authorized": true,
                    "is_playing": is_playing,
                    "track": track_name,
                    "artist": artist_name,
                    "volume_pct": st.volume,
                });
            }
        }

        let cur_wakeword = self.read_active_wakeword_name();
        let wakeword_session_active = self.wakeword_session.read().await.is_some();

        let resp = serde_json::json!({
            "status": "ok",
            "name": "GPT Mini",
            "version": "0.1.0",
            "voice": cur_voice,
            "volume": cur_vol,
            "led_color": cur_led,
            "wakeword": {
                "name": cur_wakeword,
                "session_active": wakeword_session_active,
            },
            "ip": metrics.ip,
            "uptime": metrics.uptime_formatted,
            "mem_total_mb": metrics.mem_total_mb,
            "mem_free_mb": metrics.mem_free_mb,
            "cpu_load": metrics.cpu_load,
            "spotify": spotify_info,
        });

        self.send_json(stream, &resp).await
    }

    async fn handle_api_volume(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct VolReq {
            level: u8,
        }
        let req: VolReq = serde_json::from_str(body).context("Invalid JSON body")?;
        let level = req.level.min(10);
        let new_level = volume::set_volume_level(level);

        // Cancel ducking so explicit choice is kept
        self.ducker.cancel();

        // Sync to Soloist
        if let Some(ref soloist) = self.soloist_client {
            let s = soloist.clone();
            tokio::spawn(async move {
                let _ = s.set_volume(new_level * 10).await;
            });
        }

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "level": new_level,
                "message": format!("Громкость установлена на {new_level}")
            }),
        )
        .await
    }

    async fn handle_api_voice(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct VoiceReq {
            voice: String,
        }
        let req: VoiceReq = serde_json::from_str(body).context("Invalid JSON body")?;
        let valid_voices = [
            "Callirrhoe",
            "Puck",
            "Autonoe",
            "Charon",
            "Kore",
            "Fenrir",
            "Aoede",
        ];
        if !valid_voices
            .iter()
            .any(|v| v.eq_ignore_ascii_case(&req.voice))
        {
            anyhow::bail!("Unsupported voice name: {}", req.voice);
        }

        let matched_voice = valid_voices
            .iter()
            .find(|v| v.eq_ignore_ascii_case(&req.voice))
            .unwrap()
            .to_string();

        if let Some(ref path) = self.config_path {
            update_config_voice(path, &matched_voice)?;
            info!(voice = %matched_voice, "Voice setting updated in config.toml");

            // Respawn assistant cleanly in 500ms via supervisor
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                info!("Restarting assistant process to apply new voice...");
                std::process::exit(0);
            });
        }

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "voice": matched_voice,
                "message": format!("Голос изменен на {matched_voice}. Ассистент перезапускается...")
            }),
        )
        .await
    }

    async fn handle_api_led(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct LedReq {
            color: String,
        }
        let req: LedReq = serde_json::from_str(body).context("Invalid JSON body")?;
        set_led_hardware(&req.color);
        *self.current_led_color.write().await = req.color.clone();

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "color": req.color
            }),
        )
        .await
    }

    async fn handle_api_system_restart(&self, stream: &mut TcpStream) -> Result<()> {
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "message": "Перезапуск ассистента..."
            }),
        )
        .await?;

        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            info!("Restarting assistant process by user request from Web UI");
            std::process::exit(0);
        });
        Ok(())
    }

    async fn handle_api_system_reboot(&self, stream: &mut TcpStream) -> Result<()> {
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "message": "Перезагрузка колонки..."
            }),
        )
        .await?;

        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            info!("Rebooting device by user request from Web UI");
            let _ = std::process::Command::new("/sbin/reboot").status();
            let _ = std::process::Command::new("reboot").status();
        });
        Ok(())
    }

    fn read_current_voice(&self) -> String {
        if let Some(ref path) = self.config_path {
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("voice") && trimmed.contains('=') {
                        if let Some((_, val)) = trimmed.split_once('=') {
                            let clean = val
                                .trim()
                                .trim_matches('"')
                                .trim_matches('\'')
                                .split('#')
                                .next()
                                .unwrap_or("")
                                .trim();
                            if !clean.is_empty() {
                                return clean.to_string();
                            }
                        }
                    }
                }
            }
        }
        "Callirrhoe".to_string()
    }

    async fn create_authorization_url(&self, redirect_uri: &str) -> (String, String) {
        let state = uuid::Uuid::new_v4().to_string();
        let verifier = format!("{}-{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let challenge = pkce_code_challenge(&verifier);

        let mut auths = self.pending_auths.write().await;
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

    async fn handle_spotify_callback(
        &self,
        stream: &mut TcpStream,
        query: Option<&str>,
    ) -> Result<()> {
        let params = parse_query(query.unwrap_or(""));
        let code = params.get("code").map(|s| s.as_str());
        let state = params.get("state").map(|s| s.as_str());

        match (code, state) {
            (Some(c), Some(s)) => match self.exchange_code_for_token(c, s).await {
                Ok(()) => {
                    let redirect = "HTTP/1.1 302 Found\r\nLocation: /?status=spotify_success\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    stream.write_all(redirect.as_bytes()).await?;
                }
                Err(e) => {
                    warn!(error = %e, "Failed Spotify token exchange");
                    let err_html = format!(
                        "<h1>Ошибка авторизации</h1><p>{e}</p><p><a href='/'>Назад</a></p>"
                    );
                    self.send_response(
                        stream,
                        "400 Bad Request",
                        "text/html; charset=utf-8",
                        err_html.as_bytes(),
                    )
                    .await?;
                }
            },
            _ => {
                self.send_response(
                    stream,
                    "400 Bad Request",
                    "text/plain",
                    b"Missing code or state",
                )
                .await?;
            }
        }
        Ok(())
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
            #[allow(dead_code)]
            access_token: String,
            refresh_token: Option<String>,
        }

        let token_data: TokenResp = resp.json().await.context("Failed to parse token json")?;
        let refresh_token = token_data.refresh_token.context("Missing refresh_token")?;

        info!("Spotify authorization successful! Storing refresh token");
        if let Some(ref path) = self.token_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(path, refresh_token.as_bytes())
                .context("Failed to save spotify refresh token")?;
        }

        let new_client = SpotifyApiClient::new(self.client_id.clone(), refresh_token);
        *self.spotify_client.write().await = Some(new_client);

        Ok(())
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

    async fn send_json_status(
        &self,
        stream: &mut TcpStream,
        status: &str,
        val: &serde_json::Value,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(val)?;
        self.send_response(stream, status, "application/json", &bytes)
            .await
    }

    fn read_active_wakeword_name(&self) -> String {
        if self.wakeword_model_path.exists() {
            if let Some(path_str) = self.wakeword_model_path.to_str() {
                if let Ok(ref_model) = WakewordRef::load_from_file(path_str) {
                    if !ref_model.name.is_empty() {
                        return ref_model.name;
                    }
                }
            }
        }
        "Штучка".to_string()
    }

    async fn handle_api_wakeword_session_start(
        &self,
        stream: &mut TcpStream,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct StartReq {
            name: Option<String>,
        }
        let req: StartReq = serde_json::from_str(body).unwrap_or(StartReq { name: None });
        let kw_name = req
            .name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Штучка".to_string());

        let session = EnrollmentSession::new(kw_name.clone());
        let (phase, phase_name) = EnrollmentSession::phase_for_step(1);
        *self.wakeword_session.write().await = Some(session);

        info!(keyword = %kw_name, "Started interactive wake-word enrollment session");

        let resp = serde_json::json!({
            "status": "ok",
            "keyword_name": kw_name,
            "current_step": 1,
            "target_steps": 25,
            "min_steps": 3,
            "phase": phase,
            "phase_name": phase_name,
        });
        self.send_json(stream, &resp).await
    }

    async fn handle_api_wakeword_session_status(&self, stream: &mut TcpStream) -> Result<()> {
        let lock = self.wakeword_session.read().await;
        let resp = if let Some(ref sess) = *lock {
            let (phase, phase_name) = EnrollmentSession::phase_for_step(sess.current_step);
            serde_json::json!({
                "status": "ok",
                "active": true,
                "keyword_name": sess.keyword_name,
                "current_step": sess.current_step,
                "target_steps": sess.target_steps,
                "min_steps": sess.min_steps,
                "can_compile": sess.can_compile,
                "samples_count": sess.samples.len(),
                "phase": phase,
                "phase_name": phase_name,
                "samples": sess.samples,
            })
        } else {
            serde_json::json!({
                "status": "ok",
                "active": false,
                "current_keyword": self.read_active_wakeword_name(),
            })
        };
        self.send_json(stream, &resp).await
    }

    async fn handle_api_wakeword_record_hw(&self, stream: &mut TcpStream) -> Result<()> {
        let mut session_guard = self.wakeword_session.write().await;
        let sess = match session_guard.as_mut() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({
                    "status": "error",
                    "message": "Сессия обучения не активна. Нажмите «Начать обучение»."
                });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        let recorder = match &self.hardware_recorder {
            Some(r) => r.clone(),
            None => {
                let err = serde_json::json!({
                    "status": "error",
                    "message": "Микрофон колонки недоступен (аппаратный рекордер не инициализирован)."
                });
                return self
                    .send_json_status(stream, "500 Internal Error", &err)
                    .await;
            }
        };

        // Visual indicator: pulse amber LED
        let prev_color = self.current_led_color.read().await.clone();
        set_led_hardware("amber");

        // Record 2.5 seconds of audio from Mini's hardware microphone
        let record_res = recorder.record(Duration::from_millis(2500)).await;

        // Restore LED
        set_led_hardware(&prev_color);

        let pcm = match record_res {
            Ok(p) => p,
            Err(e) => {
                let err = serde_json::json!({
                    "status": "error",
                    "message": format!("Ошибка записи микрофона: {e}")
                });
                return self
                    .send_json_status(stream, "500 Internal Error", &err)
                    .await;
            }
        };

        match sess.add_pcm_sample(&pcm) {
            Ok(sample) => {
                let (phase, phase_name) = EnrollmentSession::phase_for_step(sess.current_step);
                let resp = serde_json::json!({
                    "status": "ok",
                    "sample": sample,
                    "current_step": sess.current_step,
                    "target_steps": sess.target_steps,
                    "samples_count": sess.samples.len(),
                    "can_compile": sess.can_compile,
                    "phase": phase,
                    "phase_name": phase_name,
                });
                self.send_json(stream, &resp).await
            }
            Err(e) => {
                let err = serde_json::json!({
                    "status": "error",
                    "message": format!("{e}")
                });
                self.send_json_status(stream, "422 Unprocessable Entity", &err)
                    .await
            }
        }
    }

    async fn handle_api_wakeword_upload_sample(
        &self,
        stream: &mut TcpStream,
        body: &str,
    ) -> Result<()> {
        let mut session_guard = self.wakeword_session.write().await;
        let sess = match session_guard.as_mut() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({
                    "status": "error",
                    "message": "Сессия обучения не активна. Нажмите «Начать обучение»."
                });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        #[derive(serde::Deserialize)]
        struct UploadReq {
            wav_base64: Option<String>,
            pcm_base64: Option<String>,
        }
        let req: UploadReq = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                let err = serde_json::json!({ "status": "error", "message": format!("Неверный JSON: {e}") });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        use base64::Engine;
        let raw_pcm = if let Some(wav_b64) = req.wav_base64 {
            let wav_bytes = match base64::engine::general_purpose::STANDARD.decode(wav_b64.trim()) {
                Ok(b) => b,
                Err(e) => {
                    let err = serde_json::json!({ "status": "error", "message": format!("Base64 decode error: {e}") });
                    return self.send_json_status(stream, "400 Bad Request", &err).await;
                }
            };
            match decode_wav_pcm16(&wav_bytes) {
                Ok(p) => p,
                Err(e) => {
                    let err = serde_json::json!({ "status": "error", "message": format!("WAV decode error: {e}") });
                    return self
                        .send_json_status(stream, "422 Unprocessable Entity", &err)
                        .await;
                }
            }
        } else if let Some(pcm_b64) = req.pcm_base64 {
            let pcm_bytes = match base64::engine::general_purpose::STANDARD.decode(pcm_b64.trim()) {
                Ok(b) => b,
                Err(e) => {
                    let err = serde_json::json!({ "status": "error", "message": format!("Base64 decode error: {e}") });
                    return self.send_json_status(stream, "400 Bad Request", &err).await;
                }
            };
            let (chunks, _) = pcm_bytes.as_chunks::<2>();
            chunks.iter().map(|c| i16::from_le_bytes(*c)).collect()
        } else {
            let err = serde_json::json!({ "status": "error", "message": "Ожидалось wav_base64 или pcm_base64" });
            return self.send_json_status(stream, "400 Bad Request", &err).await;
        };

        match sess.add_pcm_sample(&raw_pcm) {
            Ok(sample) => {
                let (phase, phase_name) = EnrollmentSession::phase_for_step(sess.current_step);
                let resp = serde_json::json!({
                    "status": "ok",
                    "sample": sample,
                    "current_step": sess.current_step,
                    "target_steps": sess.target_steps,
                    "samples_count": sess.samples.len(),
                    "can_compile": sess.can_compile,
                    "phase": phase,
                    "phase_name": phase_name,
                });
                self.send_json(stream, &resp).await
            }
            Err(e) => {
                let err = serde_json::json!({ "status": "error", "message": format!("{e}") });
                self.send_json_status(stream, "422 Unprocessable Entity", &err)
                    .await
            }
        }
    }

    async fn handle_api_wakeword_remove_sample(
        &self,
        stream: &mut TcpStream,
        body: &str,
    ) -> Result<()> {
        let mut session_guard = self.wakeword_session.write().await;
        let sess = match session_guard.as_mut() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({ "status": "error", "message": "Сессия не активна" });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        #[derive(serde::Deserialize)]
        struct RemoveReq {
            id: usize,
        }
        let req: RemoveReq = serde_json::from_str(body).unwrap_or(RemoveReq { id: 0 });
        let removed = sess.remove_sample(req.id);
        let resp = serde_json::json!({
            "status": "ok",
            "removed": removed,
            "samples_count": sess.samples.len(),
            "current_step": sess.current_step,
            "can_compile": sess.can_compile,
        });
        self.send_json(stream, &resp).await
    }

    async fn handle_api_wakeword_compile(&self, stream: &mut TcpStream) -> Result<()> {
        let mut session_guard = self.wakeword_session.write().await;
        let sess = match session_guard.take() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({ "status": "error", "message": "Сессия не активна" });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        if !sess.can_compile {
            *session_guard = Some(sess);
            let err = serde_json::json!({
                "status": "error",
                "message": "Недостаточно сэмплов для компиляции (требуется минимум 3)."
            });
            return self.send_json_status(stream, "400 Bad Request", &err).await;
        }

        match sess.compile_model(&self.wakeword_model_path) {
            Ok(compile_res) => {
                // Signal hot-reload to InputGate
                if let Some(ref tx) = self.wakeword_reload_tx {
                    let _ = tx.send(self.wakeword_model_path.clone());
                    info!(path = ?self.wakeword_model_path, "Triggered hot-reload of new wake-word model");
                }

                // Visual flash: green / emerald
                set_led_hardware("emerald");

                let resp = serde_json::json!({
                    "status": "ok",
                    "message": format!("Слово «{}» успешно обучено и активировано!", compile_res.keyword_name),
                    "details": compile_res,
                });
                self.send_json(stream, &resp).await
            }
            Err(e) => {
                // Restore session on error so user doesn't lose work
                *session_guard = Some(sess);
                let err = serde_json::json!({
                    "status": "error",
                    "message": format!("Ошибка компиляции модели: {e}")
                });
                self.send_json_status(stream, "500 Internal Error", &err)
                    .await
            }
        }
    }
}

pub fn set_led_hardware(color: &str) {
    let (r, g, b) = match color {
        "azure" | "blue" => (0x00, 0xa0, 0xff),
        "purple" => (0x80, 0x00, 0xff),
        "emerald" | "green" => (0x00, 0xff, 0x60),
        "amber" | "yellow" => (0xff, 0xa0, 0x00),
        "white" => (0xff, 0xff, 0xff),
        "off" => (0x00, 0x00, 0x00),
        _ => (0x00, 0xa0, 0xff),
    };

    if r == 0 && g == 0 && b == 0 {
        let _ = std::process::Command::new("i2cset")
            .args(["-y", "0", "0x3c", "0x00", "0x00"])
            .status();
        return;
    }

    let _ = std::process::Command::new("i2cset")
        .args(["-y", "0", "0x3c", "0x00", "0x01"])
        .status();
    for reg in ["0x13", "0x14", "0x15"] {
        let _ = std::process::Command::new("i2cset")
            .args(["-y", "0", "0x3c", reg, "0xff"])
            .status();
    }
    for i in 0..12 {
        let r_reg = format!("0x{:02x}", 1 + i * 3);
        let g_reg = format!("0x{:02x}", 2 + i * 3);
        let b_reg = format!("0x{:02x}", 3 + i * 3);
        let _ = std::process::Command::new("i2cset")
            .args(["-y", "0", "0x3c", &r_reg, &format!("0x{:02x}", r)])
            .status();
        let _ = std::process::Command::new("i2cset")
            .args(["-y", "0", "0x3c", &g_reg, &format!("0x{:02x}", g)])
            .status();
        let _ = std::process::Command::new("i2cset")
            .args(["-y", "0", "0x3c", &b_reg, &format!("0x{:02x}", b)])
            .status();
    }
    let _ = std::process::Command::new("i2cset")
        .args(["-y", "0", "0x3c", "0x16", "0x00"])
        .status();
}

pub fn update_config_voice(config_path: &Path, new_voice: &str) -> Result<()> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file at {:?}", config_path))?;
    let mut updated = Vec::new();
    let mut in_assistant_section = false;
    let mut replaced = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_assistant_section = trimmed == "[assistant]";
            updated.push(line.to_string());
            continue;
        }

        if in_assistant_section
            && trimmed.starts_with("voice")
            && trimmed.contains('=')
            && !replaced
        {
            let indent = line
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>();
            updated.push(format!("{}voice = \"{}\"", indent, new_voice));
            replaced = true;
            continue;
        }

        updated.push(line.to_string());
    }

    if !replaced {
        let mut final_lines = Vec::new();
        for line in updated {
            if line.trim().starts_with("voice = ") && !replaced {
                final_lines.push(format!("voice = \"{}\"", new_voice));
                replaced = true;
            } else {
                final_lines.push(line);
            }
        }
        std::fs::write(config_path, final_lines.join("\n") + "\n")?;
    } else {
        std::fs::write(config_path, updated.join("\n") + "\n")?;
    }

    Ok(())
}

pub struct SystemMetrics {
    pub uptime_formatted: String,
    pub mem_total_mb: u64,
    pub mem_free_mb: u64,
    pub cpu_load: String,
    pub ip: String,
}

pub fn read_system_metrics(start_time: Instant) -> SystemMetrics {
    let mut mem_total_mb = 236;
    let mut mem_free_mb = 160;
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        let mut total_kb = 0;
        let mut avail_kb = 0;
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                total_kb = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            } else if line.starts_with("MemAvailable:") {
                avail_kb = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
        }
        if total_kb > 0 {
            mem_total_mb = total_kb / 1024;
        }
        if avail_kb > 0 {
            mem_free_mb = avail_kb / 1024;
        }
    }

    let cpu_load = std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "0.00".to_string());

    let uptime_formatted = if let Ok(up) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs_str) = up.split_whitespace().next() {
            if let Ok(secs_f) = secs_str.parse::<f64>() {
                let secs = secs_f as u64;
                let hrs = secs / 3600;
                let mins = (secs % 3600) / 60;
                let s = secs % 60;
                if hrs > 0 {
                    format!("{} ч {} мин", hrs, mins)
                } else {
                    format!("{} мин {} сек", mins, s)
                }
            } else {
                format!("{} сек", start_time.elapsed().as_secs())
            }
        } else {
            format!("{} сек", start_time.elapsed().as_secs())
        }
    } else {
        format!("{} сек", start_time.elapsed().as_secs())
    };

    let ip = match std::process::Command::new("ifconfig").arg("wlan0").output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(pos) = text.find("inet addr:") {
                text[pos + 10..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("10.77.0.111")
                    .to_string()
            } else {
                "10.77.0.111".to_string()
            }
        }
        Err(_) => "10.77.0.111".to_string(),
    };

    SystemMetrics {
        uptime_formatted,
        mem_total_mb,
        mem_free_mb,
        cpu_load,
        ip,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_config_voice() {
        let temp_dir = std::env::temp_dir();
        let cfg_file = temp_dir.join(format!("test_config_{}.toml", uuid::Uuid::new_v4()));
        let initial_toml = r#"
[assistant]
name = "Штучка"
voice = "Callirrhoe"
languages = ["ru-RU"]

[audio]
capture_device = "hw:0,2"
"#;
        std::fs::write(&cfg_file, initial_toml).unwrap();

        update_config_voice(&cfg_file, "Puck").unwrap();
        let updated = std::fs::read_to_string(&cfg_file).unwrap();
        assert!(updated.contains("voice = \"Puck\""));
        assert!(!updated.contains("voice = \"Callirrhoe\""));
        assert!(updated.contains("capture_device = \"hw:0,2\""));

        std::fs::remove_file(&cfg_file).unwrap();
    }

    #[test]
    fn test_url_encoding_roundtrip() {
        let orig = "https://example.com/api?query=штучка & test=1";
        let encoded = urlencoding::encode(orig);
        let decoded = urlencoding::decode(&encoded);
        assert_eq!(orig, decoded);
    }
}
