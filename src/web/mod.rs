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
use crate::spotify::{
    ensure_local_device_active, handle_web_playback_action, MusicDucker, PlaybackState,
    SoloistClient, WebSpotifyAction,
};
use crate::wakeword::enrollment::{decode_wav_pcm16, EnrollmentSession, HardwareRecorder};
pub mod auth;
use auth::{AuthManager, CalibrationLease, RequestIdentity, Scope};

use anyhow::{Context, Result};
use rustpotter::{WakewordLoad, WakewordRef, WakewordSave};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, RwLock};
use tracing::{debug, info, warn};

const DASHBOARD_HTML: &str = include_str!("dashboard.html");
const ACOUSTIC_LAB_HTML: &str = include_str!("acoustic_lab.html");
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
    wakeword_enabled: bool,
    acoustic_lab: Option<Arc<crate::acoustics::AcousticLab>>,
    auth_manager: Arc<AuthManager>,
    profiles_store: Arc<crate::acoustics::profiles::ProfilesStore>,
    samples_store: Arc<crate::acoustics::samples::SamplesStore>,
    job_runner: Arc<crate::acoustics::jobs::AcousticJobRunner>,
    session_manager: Arc<crate::acoustics::sessions::AcousticSessionManager>,
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
        let profiles_store = {
            let p = config_path
                .as_ref()
                .and_then(|cp| cp.parent())
                .unwrap_or_else(|| Path::new("."))
                .join(crate::acoustics::profiles::DEFAULT_PROFILES_FILENAME);
            Arc::new(
                crate::acoustics::profiles::ProfilesStore::new(p).unwrap_or_else(|_| {
                    crate::acoustics::profiles::ProfilesStore::new(
                        crate::acoustics::profiles::DEFAULT_PROFILES_FILENAME,
                    )
                    .unwrap()
                }),
            )
        };
        let samples_store = {
            let p = config_path
                .as_ref()
                .and_then(|cp| cp.parent())
                .unwrap_or_else(|| Path::new("."))
                .join("samples");
            Arc::new(
                crate::acoustics::samples::SamplesStore::new(p).unwrap_or_else(|_| {
                    crate::acoustics::samples::SamplesStore::new(PathBuf::from("samples")).unwrap()
                }),
            )
        };
        let job_runner = {
            let (lab, _) = crate::acoustics::AcousticLab::new(
                crate::config::WakewordCalibrationConfig::default(),
            );
            let sink = Arc::new(crate::acoustics::jobs::MockPlaybackSink::new(false));
            crate::acoustics::jobs::AcousticJobRunner::new(samples_store.clone(), lab, None, sink)
        };
        let session_manager = {
            let p = config_path
                .as_ref()
                .and_then(|cp| cp.parent())
                .unwrap_or_else(|| Path::new("."));
            Arc::new(
                crate::acoustics::sessions::AcousticSessionManager::new(p).unwrap_or_else(|_| {
                    crate::acoustics::sessions::AcousticSessionManager::new(Path::new(".")).unwrap()
                }),
            )
        };
        if let Some(rec) = session_manager.check_recovery_on_startup() {
            tracing::warn!(
                session_id = rec.session_id,
                baseline_rev = rec.baseline_revision,
                "Crash recovery record found on startup; restoring baseline and clearing recovery state"
            );
            let _ = session_manager.clear_recovery();
        }
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
            wakeword_enabled: true,
            acoustic_lab: None,
            auth_manager: Arc::new(AuthManager::default()),
            profiles_store,
            samples_store,
            job_runner,
            session_manager,
        }
    }

    pub fn with_session_manager(
        mut self,
        manager: Arc<crate::acoustics::sessions::AcousticSessionManager>,
    ) -> Self {
        self.session_manager = manager;
        self
    }

    pub fn session_manager(&self) -> Arc<crate::acoustics::sessions::AcousticSessionManager> {
        self.session_manager.clone()
    }

    pub fn with_job_runner(
        mut self,
        runner: Arc<crate::acoustics::jobs::AcousticJobRunner>,
    ) -> Self {
        self.job_runner = runner;
        self
    }

    pub fn job_runner(&self) -> Arc<crate::acoustics::jobs::AcousticJobRunner> {
        self.job_runner.clone()
    }

    pub fn with_samples_store(
        mut self,
        store: Arc<crate::acoustics::samples::SamplesStore>,
    ) -> Self {
        self.samples_store = store;
        self
    }

    pub fn samples_store(&self) -> Arc<crate::acoustics::samples::SamplesStore> {
        self.samples_store.clone()
    }

    pub fn with_profiles_store(
        mut self,
        store: Arc<crate::acoustics::profiles::ProfilesStore>,
    ) -> Self {
        self.profiles_store = store;
        self
    }

    pub fn profiles_store(&self) -> Arc<crate::acoustics::profiles::ProfilesStore> {
        self.profiles_store.clone()
    }

    pub fn with_auth_manager(mut self, auth_manager: Arc<AuthManager>) -> Self {
        self.auth_manager = auth_manager;
        self
    }

    pub fn auth_manager(&self) -> Arc<AuthManager> {
        self.auth_manager.clone()
    }

    pub fn with_acoustic_lab(mut self, lab: Arc<crate::acoustics::AcousticLab>) -> Self {
        self.acoustic_lab = Some(lab);
        self
    }

    pub fn with_token_path(mut self, path: PathBuf) -> Self {
        self.token_path = Some(path);
        self
    }

    pub fn with_initial_led_color(self, color: String) -> Self {
        if let Ok(mut lock) = self.current_led_color.try_write() {
            *lock = color;
        }
        self
    }

    pub fn current_led_color(&self) -> Arc<RwLock<String>> {
        self.current_led_color.clone()
    }

    pub fn with_wakeword_support(
        mut self,
        recorder: Option<HardwareRecorder>,
        reload_tx: Option<mpsc::UnboundedSender<PathBuf>>,
        model_path: PathBuf,
        enabled: bool,
    ) -> Self {
        self.hardware_recorder = recorder;
        self.wakeword_reload_tx = reload_tx;
        self.wakeword_model_path = model_path;
        self.wakeword_enabled = enabled;
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
        let mut content_type = String::new();
        let mut host = None;
        let mut origin = None;
        let mut referer = None;
        let mut sec_fetch_site = None;
        let mut json_content = false;
        let mut if_match = None;
        let mut auth_header = None;
        let mut idempotency_key = None;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let k_trim = k.trim();
                if k_trim.eq_ignore_ascii_case("origin") {
                    origin = Some(v.trim().to_string());
                } else if k_trim.eq_ignore_ascii_case("referer") {
                    referer = Some(v.trim().to_string());
                } else if k_trim.eq_ignore_ascii_case("sec-fetch-site") {
                    sec_fetch_site = Some(v.trim().to_ascii_lowercase());
                } else if k_trim.eq_ignore_ascii_case("content-type") {
                    content_type = v.trim().to_string();
                    json_content = content_type.split(';').next() == Some("application/json");
                } else if k_trim.eq_ignore_ascii_case("content-length") {
                    if let Ok(len) = v.trim().parse::<usize>() {
                        content_length = len.min(4 * 1024 * 1024);
                    }
                } else if k_trim.eq_ignore_ascii_case("host") {
                    host = Some(v.trim().to_string());
                } else if k_trim.eq_ignore_ascii_case("if-match") {
                    if_match = Some(v.trim().to_string());
                } else if k_trim.eq_ignore_ascii_case("authorization") {
                    auth_header = Some(v.trim().to_string());
                } else if k_trim.eq_ignore_ascii_case("idempotency-key") {
                    idempotency_key = Some(v.trim().to_string());
                }
            }
        }

        let mut full_body_bytes = Vec::new();
        let body_owned: String;
        let body: &str = if let Some((pos, delim_len)) = header_end {
            let body_start = pos + delim_len;
            let already_read = n.saturating_sub(body_start);
            let target_len = if content_length > 0 {
                content_length
            } else {
                already_read
            };
            if target_len <= 4 * 1024 * 1024 {
                full_body_bytes.reserve(target_len);
                let initial_take = already_read.min(target_len);
                full_body_bytes.extend_from_slice(&buf[body_start..body_start + initial_take]);
                let mut remaining = target_len.saturating_sub(already_read);
                let mut chunk_buf = [0u8; 16384];
                while remaining > 0 {
                    let to_read = remaining.min(chunk_buf.len());
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        stream.read(&mut chunk_buf[..to_read]),
                    )
                    .await
                    {
                        Ok(Ok(read_bytes)) if read_bytes > 0 => {
                            full_body_bytes.extend_from_slice(&chunk_buf[..read_bytes]);
                            remaining -= read_bytes;
                        }
                        _ => break,
                    }
                }
                body_owned = String::from_utf8_lossy(&full_body_bytes).to_string();
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

        let host_matches_url = |url_val: &str, h: &str| {
            let rest = url_val
                .strip_prefix("http://")
                .or_else(|| url_val.strip_prefix("https://"));
            rest.is_some_and(|r| r == h || r.starts_with(&format!("{h}/")))
        };
        let same_origin = origin
            .as_ref()
            .is_some_and(|o| host.as_ref().is_some_and(|h| host_matches_url(o, h)))
            || ((method == "GET" || method == "HEAD")
                && (sec_fetch_site.as_deref() == Some("same-origin")
                    || referer
                        .as_ref()
                        .is_some_and(|r| host.as_ref().is_some_and(|h| host_matches_url(r, h)))));

        let identity = match self
            .auth_manager
            .authenticate(auth_header.as_deref(), same_origin)
        {
            Ok(ident) => ident,
            Err(err) => {
                return self
                    .send_json_status(
                        &mut stream,
                        err.status_code(),
                        &serde_json::json!({
                            "status": "error",
                            "message": err.message()
                        }),
                    )
                    .await;
            }
        };

        if let Err((code, msg)) = self.auth_manager.authorize_route(&identity, method, path) {
            return self
                .send_json_status(
                    &mut stream,
                    code,
                    &serde_json::json!({
                        "status": "error",
                        "message": msg
                    }),
                )
                .await;
        }

        if (method == "POST" || method == "PUT" || method == "DELETE")
            && (path.starts_with("/api/v1/acoustics") || path.starts_with("/api/v1/calibration"))
        {
            let is_authorized_caller = same_origin
                || identity.is_owner()
                || matches!(identity, RequestIdentity::Calibration { .. });
            let is_restore =
                path == "/api/v1/acoustics/restore" || path == "/api/v1/calibration/restore";
            let is_delete_or_empty = method == "DELETE"
                || (method == "POST"
                    && (path.ends_with("/archive")
                        || path.ends_with("/activate")
                        || path.ends_with("/export")));
            let is_wav_upload = (path == "/api/v1/acoustics/samples"
                || path == "/api/v1/calibration/samples")
                && (content_type.starts_with("audio/wav")
                    || content_type.starts_with("application/octet-stream"));
            if !is_authorized_caller
                || (!is_restore && !is_delete_or_empty && !json_content && !is_wav_upload)
            {
                return self
                    .send_json_status(
                        &mut stream,
                        "403 Forbidden",
                        &serde_json::json!({"status":"error","message":"Acoustic updates require JSON or WAV"}),
                    )
                    .await;
            }
            let max_payload = if path.contains("/samples") {
                4 * 1024 * 1024
            } else {
                2 * 1024 * 1024
            };
            if body.len() > max_payload || full_body_bytes.len() > max_payload {
                return self
                    .send_json_status(
                        &mut stream,
                        "413 Content Too Large",
                        &serde_json::json!({"status":"error","message":"Acoustic payload too large"}),
                    )
                    .await;
            }
        }
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

            ("GET", "/acoustics") | ("GET", "/acoustics/") => {
                self.send_response(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    ACOUSTIC_LAB_HTML.as_bytes(),
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
                self.handle_api_wakeword_compile(&mut stream, body).await?;
            }
            ("POST", "/api/v1/wakeword/rename") | ("POST", "/api/v1/wakeword/name") => {
                self.handle_api_wakeword_rename(&mut stream, body).await?;
            }
            ("GET", "/api/v1/wakeword/analysis") | ("GET", "/api/v1/wakeword/session/analysis") => {
                self.handle_api_wakeword_analysis(&mut stream).await?;
            }
            ("GET", "/api/v1/acoustics")
            | ("GET", "/api/v1/acoustics/state")
            | ("GET", "/api/v1/calibration")
            | ("GET", "/api/v1/calibration/state") => {
                self.handle_api_get_calibration(&mut stream).await?;
            }
            ("GET", "/api/v1/acoustics/schema")
            | ("GET", "/api/v1/calibration/schema")
            | ("GET", "/api/v1/acoustics/openapi.json")
            | ("GET", "/api/v1/calibration/openapi.json") => {
                self.handle_api_get_openapi(&mut stream).await?;
            }
            ("POST", "/api/v1/acoustics") | ("POST", "/api/v1/calibration") => {
                self.handle_api_post_calibration(&mut stream, body, if_match.as_deref(), &identity)
                    .await?;
            }
            ("POST", "/api/v1/acoustics/restore") | ("POST", "/api/v1/calibration/restore") => {
                self.handle_api_restore_calibration(&mut stream).await?;
            }
            ("GET", "/api/v1/acoustics/events") | ("GET", "/api/v1/calibration/events") => {
                self.handle_api_get_acoustic_events(&mut stream, query)
                    .await?;
            }
            ("GET", "/api/v1/acoustics/aec") | ("GET", "/api/v1/calibration/aec") => {
                self.handle_api_get_aec(&mut stream).await?;
            }
            ("POST", "/api/v1/acoustics/aec") | ("POST", "/api/v1/calibration/aec") => {
                self.handle_api_post_aec(&mut stream, body, &identity)
                    .await?;
            }
            ("POST", "/api/v1/acoustics/test_session")
            | ("POST", "/api/v1/calibration/test_session") => {
                self.handle_api_post_acoustic_test_session(&mut stream, body)
                    .await?;
            }
            ("POST", "/api/v1/acoustics/tokens") | ("POST", "/api/v1/calibration/tokens") => {
                self.handle_api_post_acoustic_tokens(&mut stream, body)
                    .await?;
            }
            ("POST", "/api/v1/acoustics/tokens/revoke")
            | ("POST", "/api/v1/calibration/tokens/revoke") => {
                self.handle_api_post_acoustic_tokens_revoke(&mut stream, body)
                    .await?;
            }
            ("POST", "/api/v1/acoustics/lease/revoke")
            | ("POST", "/api/v1/calibration/lease/revoke") => {
                self.handle_api_post_acoustic_lease_revoke(&mut stream, body)
                    .await?;
            }

            // Room Profiles and Candidate Preview
            ("GET", "/api/v1/acoustics/profiles") | ("GET", "/api/v1/calibration/profiles") => {
                self.handle_api_get_profiles(&mut stream, query).await?;
            }
            ("POST", "/api/v1/acoustics/profiles") | ("POST", "/api/v1/calibration/profiles") => {
                self.handle_api_post_profiles(
                    &mut stream,
                    body,
                    idempotency_key.as_deref(),
                    &identity,
                )
                .await?;
            }
            ("POST", "/api/v1/acoustics/profiles/export")
            | ("POST", "/api/v1/calibration/profiles/export") => {
                self.handle_api_post_profiles_export(&mut stream).await?;
            }
            ("POST", "/api/v1/acoustics/profiles/import")
            | ("POST", "/api/v1/calibration/profiles/import") => {
                self.handle_api_post_profiles_import(&mut stream, body, &identity)
                    .await?;
            }
            ("POST", "/api/v1/acoustics/preview") | ("POST", "/api/v1/calibration/preview") => {
                self.handle_api_post_preview(&mut stream, body).await?;
            }
            (m, p)
                if p.starts_with("/api/v1/acoustics/profiles/")
                    || p.starts_with("/api/v1/calibration/profiles/") =>
            {
                let sub = p
                    .strip_prefix("/api/v1/acoustics/profiles/")
                    .or_else(|| p.strip_prefix("/api/v1/calibration/profiles/"))
                    .unwrap_or("");
                if let Some(profile_id) = sub.strip_suffix("/activate") {
                    if m == "POST" {
                        self.handle_api_post_profile_activate(&mut stream, profile_id)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(profile_id) = sub.strip_suffix("/archive") {
                    if m == "POST" {
                        self.handle_api_post_profile_archive(&mut stream, profile_id)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(profile_id) = sub.strip_suffix("/rename") {
                    if m == "POST" {
                        self.handle_api_post_profile_rename(&mut stream, profile_id, body)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if m == "GET" {
                    self.handle_api_get_profile_by_id(&mut stream, sub).await?;
                } else if m == "PUT" || m == "POST" {
                    self.handle_api_put_profile_by_id(
                        &mut stream,
                        sub,
                        body,
                        if_match.as_deref(),
                        &identity,
                    )
                    .await?;
                } else if m == "DELETE" {
                    self.handle_api_delete_profile_by_id(&mut stream, sub)
                        .await?;
                } else {
                    self.send_not_found(&mut stream).await?;
                }
            }

            // Samples Library
            ("GET", "/api/v1/acoustics/samples") | ("GET", "/api/v1/calibration/samples") => {
                self.handle_api_list_samples(&mut stream, query).await?;
            }
            ("POST", "/api/v1/acoustics/samples") | ("POST", "/api/v1/calibration/samples") => {
                self.handle_api_create_sample(
                    &mut stream,
                    body,
                    &content_type,
                    &full_body_bytes,
                    query,
                )
                .await?;
            }
            (m, p)
                if p.starts_with("/api/v1/acoustics/samples/")
                    || p.starts_with("/api/v1/calibration/samples/") =>
            {
                let sub = p
                    .strip_prefix("/api/v1/acoustics/samples/")
                    .or_else(|| p.strip_prefix("/api/v1/calibration/samples/"))
                    .unwrap_or("");
                if let Some(sample_id) = sub
                    .strip_suffix("/wav")
                    .or_else(|| sub.strip_suffix("/audio"))
                {
                    if m == "GET" {
                        self.handle_api_get_sample_wav(&mut stream, sample_id)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(sample_id) = sub.strip_suffix("/spectrogram") {
                    if m == "GET" {
                        self.handle_api_get_sample_spectrogram(&mut stream, sample_id)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(sample_id) = sub.strip_suffix("/archive") {
                    if m == "POST" {
                        self.handle_api_archive_sample(&mut stream, sample_id)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if m == "GET" {
                    self.handle_api_get_sample(&mut stream, sub).await?;
                } else if m == "PUT" || m == "POST" {
                    self.handle_api_update_sample(&mut stream, sub, body)
                        .await?;
                } else if m == "DELETE" {
                    self.handle_api_delete_sample(&mut stream, sub).await?;
                } else {
                    self.send_not_found(&mut stream).await?;
                }
            }

            // Acoustic Jobs Runner
            ("GET", "/api/v1/acoustics/jobs") | ("GET", "/api/v1/calibration/jobs") => {
                self.handle_api_list_jobs(&mut stream).await?;
            }
            ("POST", "/api/v1/acoustics/jobs") | ("POST", "/api/v1/calibration/jobs") => {
                self.handle_api_create_job(
                    &mut stream,
                    body,
                    idempotency_key.as_deref(),
                    &identity,
                )
                .await?;
            }
            (m, p)
                if p.starts_with("/api/v1/acoustics/jobs/")
                    || p.starts_with("/api/v1/calibration/jobs/") =>
            {
                let sub = p
                    .strip_prefix("/api/v1/acoustics/jobs/")
                    .or_else(|| p.strip_prefix("/api/v1/calibration/jobs/"))
                    .unwrap_or("");
                if let Some(job_id) = sub.strip_suffix("/cancel") {
                    if m == "POST" {
                        self.handle_api_cancel_job(&mut stream, job_id, body)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(job_id) = sub.strip_suffix("/report") {
                    if m == "GET" {
                        self.handle_api_get_job_report(&mut stream, job_id).await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(job_id) = sub.strip_suffix("/events") {
                    if m == "GET" {
                        self.handle_api_get_job_events(&mut stream, job_id).await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if m == "GET" {
                    self.handle_api_get_job(&mut stream, sub).await?;
                } else {
                    self.send_not_found(&mut stream).await?;
                }
            }

            // Acoustic Evaluation & Search
            ("POST", "/api/v1/acoustics/evaluate") | ("POST", "/api/v1/calibration/evaluate") => {
                self.handle_api_evaluate(&mut stream, body).await?;
            }
            ("POST", "/api/v1/acoustics/search") | ("POST", "/api/v1/calibration/search") => {
                self.handle_api_search(&mut stream, body).await?;
            }
            ("POST", "/api/v1/acoustics/cue") | ("POST", "/api/v1/calibration/cue") => {
                self.handle_api_post_cue(&mut stream, body, &identity)
                    .await?;
            }
            ("GET", "/api/v1/acoustics/volume") | ("GET", "/api/v1/calibration/volume") => {
                self.handle_api_get_acoustics_volume(&mut stream, &identity)
                    .await?;
            }
            ("POST", "/api/v1/acoustics/volume") | ("POST", "/api/v1/calibration/volume") => {
                self.handle_api_post_acoustics_volume(&mut stream, body, &identity)
                    .await?;
            }
            ("GET", "/api/v1/acoustics/reference_playback")
            | ("GET", "/api/v1/calibration/reference_playback") => {
                self.handle_api_get_reference_playback(&mut stream).await?;
            }
            ("POST", "/api/v1/acoustics/reference_playback")
            | ("POST", "/api/v1/calibration/reference_playback") => {
                self.handle_api_post_reference_playback(&mut stream, body, &identity)
                    .await?;
            }

            // Acoustic Calibration Sessions
            ("GET", "/api/v1/acoustics/sessions") | ("GET", "/api/v1/calibration/sessions") => {
                self.handle_api_get_session(&mut stream, None).await?;
            }
            ("POST", "/api/v1/acoustics/sessions") | ("POST", "/api/v1/calibration/sessions") => {
                self.handle_api_create_session(&mut stream, body).await?;
            }
            (m, p)
                if p.starts_with("/api/v1/acoustics/sessions/")
                    || p.starts_with("/api/v1/calibration/sessions/") =>
            {
                let sub = p
                    .strip_prefix("/api/v1/acoustics/sessions/")
                    .or_else(|| p.strip_prefix("/api/v1/calibration/sessions/"))
                    .unwrap_or("");
                if let Some(sess_id) = sub.strip_suffix("/candidate") {
                    if m == "POST" {
                        self.handle_api_session_candidate(&mut stream, sess_id, body)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(sess_id) = sub.strip_suffix("/restore") {
                    if m == "POST" {
                        self.handle_api_session_restore(&mut stream, sess_id)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(sess_id) = sub.strip_suffix("/confirm_owner") {
                    if m == "POST" {
                        self.handle_api_session_confirm_owner(&mut stream, sess_id, body)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(sess_id) = sub.strip_suffix("/finish") {
                    if m == "POST" {
                        self.handle_api_session_finish(&mut stream, sess_id, body, &identity)
                            .await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if let Some(sess_id) = sub.strip_suffix("/cancel") {
                    if m == "POST" {
                        self.handle_api_session_cancel(&mut stream, sess_id).await?;
                    } else {
                        self.send_not_found(&mut stream).await?;
                    }
                } else if m == "GET" {
                    self.handle_api_get_session(&mut stream, Some(sub)).await?;
                } else {
                    self.send_not_found(&mut stream).await?;
                }
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

            // Spotify Playback API (prioritizes GPT Mini receiver over external devices)
            (
                "POST",
                p @ ("/api/spotify/pause"
                | "/api/spotify/play"
                | "/api/spotify/next"
                | "/api/spotify/previous"
                | "/api/spotify/activate"),
            ) => {
                let action = match p {
                    "/api/spotify/pause" => WebSpotifyAction::Pause,
                    "/api/spotify/play" => WebSpotifyAction::Play,
                    "/api/spotify/next" => WebSpotifyAction::Next,
                    "/api/spotify/previous" => WebSpotifyAction::Previous,
                    _ => WebSpotifyAction::Activate,
                };
                let res = handle_web_playback_action(
                    self.soloist_client.as_deref(),
                    &self.spotify_client,
                    action,
                )
                .await;
                self.send_json(&mut stream, &serde_json::json!(res)).await?;
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
        let (cur_provider, cur_voice) = self.read_current_voice_and_provider();
        let cur_led = self.current_led_color.read().await.clone();

        // Query Spotify Soloist track
        let mut spotify_info = serde_json::json!({
            "authorized": self.spotify_client.read().await.is_some(),
            "reachable": false,
            "is_active": false,
            "is_playing": false,
            "track": null,
            "artist": null,
        });

        if let Some(ref soloist) = self.soloist_client {
            if let Ok(st) = soloist.get_status().await {
                let is_playing = st.is_actually_playing();
                let track_name = st.track.as_ref().map(|t| t.name.clone());
                let artist_name = st.track.as_ref().map(|t| t.artist.clone());
                spotify_info = serde_json::json!({
                    "authorized": true,
                    "reachable": true,
                    "is_active": st.is_active,
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
            "provider": cur_provider,
            "voice": cur_voice,
            "volume": cur_vol,
            "led_color": cur_led,
            "wakeword": {
                "name": cur_wakeword,
                "enabled": self.wakeword_enabled,
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
                let _ = s
                    .set_volume(volume::soloist_percent_for_level(new_level))
                    .await;
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
            provider: Option<String>,
            voice: Option<String>,
        }
        let req: VoiceReq = serde_json::from_str(body).context("Invalid JSON body")?;

        let (cur_provider, _cur_voice) = self.read_current_voice_and_provider();
        let target_provider = req
            .provider
            .as_deref()
            .map(|p| p.to_lowercase())
            .unwrap_or(cur_provider);

        let valid_gemini_voices = [
            "Callirrhoe",
            "Puck",
            "Autonoe",
            "Charon",
            "Kore",
            "Fenrir",
            "Aoede",
        ];
        let valid_openai_voices = [
            "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse",
        ];

        let target_voice = if let Some(ref v) = req.voice {
            v.trim().to_string()
        } else if target_provider == "openai" {
            "alloy".to_string()
        } else {
            "Callirrhoe".to_string()
        };

        let matched_voice = match target_provider.as_str() {
            "openai" => {
                let has_openai_key = std::env::var("OPENAI_API_KEY")
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                if !has_openai_key {
                    let err = serde_json::json!({
                        "status": "error",
                        "message": "Для использования OpenAI необходимо указать OPENAI_API_KEY в файле .env на устройстве"
                    });
                    return self.send_json_status(stream, "400 Bad Request", &err).await;
                }
                if !valid_openai_voices
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(&target_voice))
                {
                    anyhow::bail!("Unsupported OpenAI voice: {}", target_voice);
                }
                valid_openai_voices
                    .iter()
                    .find(|v| v.eq_ignore_ascii_case(&target_voice))
                    .unwrap()
                    .to_string()
            }
            _ => {
                if !valid_gemini_voices
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(&target_voice))
                {
                    anyhow::bail!("Unsupported Gemini voice: {}", target_voice);
                }
                valid_gemini_voices
                    .iter()
                    .find(|v| v.eq_ignore_ascii_case(&target_voice))
                    .unwrap()
                    .to_string()
            }
        };

        let config_path = self.resolve_config_path();
        if let Some(ref path) = config_path {
            update_config_voice_and_provider(path, Some(&target_provider), Some(&matched_voice))?;
            info!(
                provider = %target_provider,
                voice = %matched_voice,
                path = %path.display(),
                "Voice & provider setting updated in config.toml"
            );

            // Respawn assistant cleanly in 500ms via supervisor
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                info!("Restarting assistant process to apply new voice & provider...");
                std::process::exit(0);
            });
        }

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "provider": target_provider,
                "voice": matched_voice,
                "message": format!("Модель {} ({}) применена. Ассистент перезапускается...", target_provider, matched_voice)
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

        if let Some(ref path) = self.config_path {
            if let Err(e) = update_config_led_color(path, &req.color) {
                warn!("Failed to persist led_color to config: {}", e);
            }
        }

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

    async fn handle_api_get_calibration(&self, stream: &mut TcpStream) -> Result<()> {
        match &self.acoustic_lab {
            Some(lab) => {
                let snap = lab.snapshot();
                let etag = format!("\"{}\"", snap.revision);
                self.send_json_with_etag(stream, &lab.response(), &etag)
                    .await
            }
            None => {
                self.send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Audio loop is unavailable"}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_calibration(
        &self,
        stream: &mut TcpStream,
        body: &str,
        if_match: Option<&str>,
        identity: &RequestIdentity,
    ) -> Result<()> {
        if let RequestIdentity::Calibration { token, .. } = identity {
            if let Err((code, msg)) = self.auth_manager.record_run(token) {
                return self
                    .send_json_status(
                        stream,
                        code,
                        &serde_json::json!({
                            "status": "error",
                            "message": msg
                        }),
                    )
                    .await;
            }
        }
        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Audio loop is unavailable"}),
                )
                .await;
        };
        let snapshot = lab.snapshot();
        if let Some(im) = if_match {
            let im_clean = im.trim().trim_matches('"');
            let current_rev_str = snapshot.revision.to_string();
            let current_full_tag =
                format!("{}:{}", snapshot.runtime_instance_id, snapshot.revision);
            if im_clean != current_rev_str && im_clean != current_full_tag {
                return self
                    .send_json_status(
                        stream,
                        "412 Precondition Failed",
                        &serde_json::json!({
                            "status": "error",
                            "message": "If-Match condition failed; acoustic profile revision mismatch",
                            "snapshot": lab.response()
                        }),
                    )
                    .await;
            }
        }
        let payload: crate::acoustics::ApplyPayload = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Invalid acoustic profile payload: {e}")
                        }),
                    )
                    .await;
            }
        };
        if let Err(e) = payload.settings.validate() {
            return self
                .send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({"status":"error","message":e.to_string()}),
                )
                .await;
        }
        if let Some(ref inst) = payload.expected_instance_id {
            if inst != &snapshot.runtime_instance_id {
                return self
                    .send_json_status(
                        stream,
                        "409 Conflict",
                        &serde_json::json!({
                            "status": "error",
                            "message": "Instance ID mismatch; GPT Mini was restarted. Reload status before updating.",
                            "snapshot": lab.response()
                        }),
                    )
                    .await;
            }
        }
        if payload.expected_revision != snapshot.revision || snapshot.pending {
            return self
                .send_json_status(
                    stream,
                    "409 Conflict",
                    &serde_json::json!({
                        "status": "error",
                        "message": "Профиль изменился или ожидает применения. Обновите статус.",
                        "snapshot": lab.response()
                    }),
                )
                .await;
        }
        let Some(path) = self.resolve_config_path() else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Config path is unavailable; nothing saved"}),
                )
                .await;
        };
        match lab.apply(&path, payload).await {
            Ok(new_snap) => {
                let etag = format!("\"{}\"", new_snap.revision);
                self.send_json_with_etag(stream, &lab.response(), &etag)
                    .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":e.to_string(),"snapshot":lab.response()}),
                )
                .await
            }
        }
    }

    async fn handle_api_restore_calibration(&self, stream: &mut TcpStream) -> Result<()> {
        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Audio loop is unavailable"}),
                )
                .await;
        };
        let Some(path) = self.resolve_config_path() else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Config path is unavailable"}),
                )
                .await;
        };
        match lab.restore_working(&path).await {
            Ok(snap) => {
                let etag = format!("\"{}\"", snap.revision);
                self.send_json_with_etag(stream, &lab.response(), &etag)
                    .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({"status":"error","message":e.to_string(),"snapshot":lab.response()}),
                )
                .await
            }
        }
    }

    async fn handle_api_get_acoustic_events(
        &self,
        stream: &mut TcpStream,
        query: Option<&str>,
    ) -> Result<()> {
        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Audio loop is unavailable"}),
                )
                .await;
        };
        let mut since: u64 = 0;
        let mut limit: usize = 50;
        if let Some(q) = query {
            for pair in q.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    match k {
                        "since" => since = v.parse().unwrap_or(0),
                        "limit" => limit = v.parse().unwrap_or(50),
                        _ => {}
                    }
                }
            }
        }
        let events = lab.events(since, limit);
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "since": since,
                "limit": limit,
                "events": events,
            }),
        )
        .await
    }

    async fn handle_api_get_aec(&self, stream: &mut TcpStream) -> Result<()> {
        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Audio loop is unavailable"}),
                )
                .await;
        };
        let Some(snap) = lab.aec_snapshot() else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"AEC telemetry is not attached (capture inactive)"}),
                )
                .await;
        };
        let mut resp = serde_json::to_value(&snap)?;
        if let Some(obj) = resp.as_object_mut() {
            obj.insert("status".into(), serde_json::json!("ok"));
        }
        self.send_json(stream, &resp).await
    }

    async fn handle_api_post_aec(
        &self,
        stream: &mut TcpStream,
        body: &str,
        identity: &RequestIdentity,
    ) -> Result<()> {
        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Audio loop is unavailable"}),
                )
                .await;
        };
        #[derive(serde::Deserialize)]
        struct AecTogglePayload {
            enabled: bool,
            #[serde(default)]
            persist: Option<bool>,
        }
        let payload: AecTogglePayload = match serde_json::from_str(body) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status":"error","message":format!("Invalid JSON: {e}")}),
                    )
                    .await;
            }
        };
        let persist = payload.persist.unwrap_or(false);
        if persist && !identity.is_owner() {
            return self
                .send_json_status(
                    stream,
                    "403 Forbidden",
                    &serde_json::json!({"status":"error","message":"Forbidden: calibration identity cannot persist startup [audio.aec] settings"}),
                )
                .await;
        }
        let Some(updated) = lab.set_aec_enabled(payload.enabled) else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"AEC telemetry is not attached (capture inactive)"}),
                )
                .await;
        };
        if persist {
            if let Some(cfg_path) = self.resolve_config_path() {
                if cfg_path.exists() {
                    if let Err(e) = crate::acoustics::save_aec_enabled(&cfg_path, payload.enabled) {
                        return self
                            .send_json_status(
                                stream,
                                "500 Internal Server Error",
                                &serde_json::json!({"status":"error","message":format!("Failed to persist AEC state: {e}")}),
                            )
                            .await;
                    }
                }
            }
        }
        let mut resp = serde_json::to_value(&updated)?;
        if let Some(obj) = resp.as_object_mut() {
            obj.insert("status".into(), serde_json::json!("ok"));
        }
        self.send_json(stream, &resp).await
    }

    async fn handle_api_post_acoustic_test_session(
        &self,
        stream: &mut TcpStream,
        body: &str,
    ) -> Result<()> {
        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status":"error","message":"Audio loop is unavailable"}),
                )
                .await;
        };
        #[derive(serde::Deserialize)]
        struct TestSessionPayload {
            active: bool,
        }
        let payload: TestSessionPayload = match serde_json::from_str(body) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status":"error","message":format!("Invalid JSON: {e}")}),
                    )
                    .await;
            }
        };
        lab.set_test_session(payload.active);
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "test_session": payload.active,
            }),
        )
        .await
    }

    async fn handle_api_post_acoustic_tokens(
        &self,
        stream: &mut TcpStream,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct CreateTokenRequest {
            #[serde(default = "default_identity_id")]
            identity_id: String,
            #[serde(default = "default_ttl_secs")]
            ttl_secs: u64,
            #[serde(default)]
            scopes: Vec<String>,
            lease: Option<LeaseRequest>,
        }
        #[derive(serde::Deserialize)]
        struct LeaseRequest {
            #[serde(default = "default_max_duration")]
            max_duration_secs: u64,
            #[serde(default = "default_max_vol")]
            max_volume_pct: u8,
            #[serde(default = "default_max_runs")]
            max_runs: u32,
        }
        fn default_identity_id() -> String {
            "calibration-agent".to_string()
        }
        fn default_ttl_secs() -> u64 {
            3600
        }
        fn default_max_duration() -> u64 {
            600
        }
        fn default_max_vol() -> u8 {
            60
        }
        fn default_max_runs() -> u32 {
            50
        }

        let req: CreateTokenRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status":"error","message":format!("Invalid request body: {e}")}),
                    )
                    .await;
            }
        };

        let mut scopes_set = HashSet::new();
        if req.scopes.is_empty() {
            scopes_set = Scope::all();
        } else {
            for s in &req.scopes {
                match s.as_str() {
                    "acoustics_read" => {
                        scopes_set.insert(Scope::AcousticsRead);
                    }
                    "acoustics_write" => {
                        scopes_set.insert(Scope::AcousticsWrite);
                    }
                    "profiles" => {
                        scopes_set.insert(Scope::Profiles);
                    }
                    "samples" => {
                        scopes_set.insert(Scope::Samples);
                    }
                    "jobs" => {
                        scopes_set.insert(Scope::Jobs);
                    }
                    other => {
                        return self
                            .send_json_status(
                                stream,
                                "400 Bad Request",
                                &serde_json::json!({"status":"error","message":format!("Unknown scope: {other}")}),
                            )
                            .await;
                    }
                }
            }
        }

        let baseline_rev = self
            .acoustic_lab
            .as_ref()
            .map(|l| l.snapshot().revision)
            .unwrap_or(0);
        let lease_req = req.lease.unwrap_or(LeaseRequest {
            max_duration_secs: default_max_duration(),
            max_volume_pct: default_max_vol(),
            max_runs: default_max_runs(),
        });
        let lease = CalibrationLease {
            lease_id: uuid::Uuid::new_v4().to_string(),
            baseline_revision: baseline_rev,
            max_duration_secs: lease_req.max_duration_secs,
            max_volume_pct: lease_req.max_volume_pct,
            max_runs: lease_req.max_runs,
            runs_count: 0,
            active: true,
        };

        let ttl = Duration::from_secs(req.ttl_secs.clamp(60, 86400));
        let (token_secret, cal_token) = self.auth_manager.issue_calibration_token(
            req.identity_id.clone(),
            ttl,
            scopes_set,
            lease.clone(),
        );

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "token": token_secret,
                "identity_id": req.identity_id,
                "expires_in_secs": req.ttl_secs,
                "scopes": cal_token.scopes,
                "lease": lease,
            }),
        )
        .await
    }

    async fn handle_api_post_acoustic_tokens_revoke(
        &self,
        stream: &mut TcpStream,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct RevokeTokenRequest {
            token: String,
        }
        let req: RevokeTokenRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status":"error","message":format!("Invalid request body: {e}")}),
                    )
                    .await;
            }
        };
        let revoked = self.auth_manager.revoke_token(&req.token);
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "revoked": revoked,
            }),
        )
        .await
    }

    async fn handle_api_post_acoustic_lease_revoke(
        &self,
        stream: &mut TcpStream,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct RevokeLeaseRequest {
            lease_id: String,
        }
        let req: RevokeLeaseRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status":"error","message":format!("Invalid request body: {e}")}),
                    )
                    .await;
            }
        };
        let revoked = self.auth_manager.revoke_lease(&req.lease_id);
        if let Some(ref lab) = self.acoustic_lab {
            lab.set_test_session(false);
        }
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "lease_revoked": revoked,
            }),
        )
        .await
    }

    async fn handle_api_get_profiles(
        &self,
        stream: &mut TcpStream,
        query: Option<&str>,
    ) -> Result<()> {
        let include_archived = query
            .map(|q| q.contains("include_archived=true"))
            .unwrap_or(false);
        match self.profiles_store.list(include_archived) {
            Ok(profiles) => {
                let active_id = self
                    .profiles_store
                    .get_active()
                    .ok()
                    .flatten()
                    .map(|p| p.id);
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "active_profile_id": active_id,
                        "profiles": profiles,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({"status": "error", "message": format!("Failed to list profiles: {e}")}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_profiles(
        &self,
        stream: &mut TcpStream,
        body: &str,
        idempotency_key: Option<&str>,
        identity: &RequestIdentity,
    ) -> Result<()> {
        let dto: crate::acoustics::profiles::CreateProfileDto = match serde_json::from_str(body) {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status": "error", "message": format!("Invalid profile payload: {e}")}),
                    )
                    .await;
            }
        };

        let is_owner = identity.is_owner();
        match self.profiles_store.create(dto, idempotency_key, is_owner) {
            Ok(profile) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "profile": profile,
                    }),
                )
                .await
            }
            Err(e) => {
                let err_msg = e.to_string();
                let status_code = if err_msg.contains("Only the owner can set owner_confirmed") {
                    "403 Forbidden"
                } else if err_msg.contains("already exists") {
                    "409 Conflict"
                } else {
                    "400 Bad Request"
                };
                self.send_json_status(
                    stream,
                    status_code,
                    &serde_json::json!({"status": "error", "message": err_msg}),
                )
                .await
            }
        }
    }

    async fn handle_api_get_profile_by_id(&self, stream: &mut TcpStream, id: &str) -> Result<()> {
        match self.profiles_store.get(id) {
            Ok(Some(profile)) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "profile": profile,
                    }),
                )
                .await
            }
            Ok(None) => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({"status": "error", "message": format!("Profile '{id}' not found")}),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({"status": "error", "message": e.to_string()}),
                )
                .await
            }
        }
    }

    async fn handle_api_put_profile_by_id(
        &self,
        stream: &mut TcpStream,
        id: &str,
        body: &str,
        if_match: Option<&str>,
        identity: &RequestIdentity,
    ) -> Result<()> {
        let dto: crate::acoustics::profiles::UpdateProfileDto = match serde_json::from_str(body) {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status": "error", "message": format!("Invalid profile update payload: {e}")}),
                    )
                    .await;
            }
        };

        let if_match_rev = if_match.and_then(|im| im.trim().trim_matches('"').parse::<u64>().ok());
        let is_owner = identity.is_owner();

        match self.profiles_store.update(id, dto, if_match_rev, is_owner) {
            Ok(updated) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "profile": updated,
                    }),
                )
                .await
            }
            Err(e) => {
                let err_msg = e.to_string();
                let status_code = if err_msg.contains("Only the owner can set owner_confirmed") {
                    "403 Forbidden"
                } else if err_msg.contains("Precondition Failed") {
                    "412 Precondition Failed"
                } else if err_msg.contains("not found") {
                    "404 Not Found"
                } else {
                    "400 Bad Request"
                };
                self.send_json_status(
                    stream,
                    status_code,
                    &serde_json::json!({"status": "error", "message": err_msg}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_profile_activate(
        &self,
        stream: &mut TcpStream,
        id: &str,
    ) -> Result<()> {
        let profile = match self.profiles_store.get(id) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return self
                    .send_json_status(
                        stream,
                        "404 Not Found",
                        &serde_json::json!({"status": "error", "message": format!("Profile '{id}' not found")}),
                    )
                    .await;
            }
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "500 Internal Server Error",
                        &serde_json::json!({"status": "error", "message": e.to_string()}),
                    )
                    .await;
            }
        };

        if profile.archived {
            return self
                .send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({"status": "error", "message": "Cannot activate archived profile"}),
                )
                .await;
        }

        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status": "error", "message": "Audio loop is unavailable"}),
                )
                .await;
        };

        let config_path = self
            .resolve_config_path()
            .unwrap_or_else(|| PathBuf::from("config.toml"));

        let current_snap = lab.snapshot();
        let payload = crate::acoustics::ApplyPayload {
            expected_revision: current_snap.revision,
            settings: profile.settings.clone(),
            expected_instance_id: None,
        };

        match lab.apply(&config_path, payload).await {
            Ok(_) => {
                let _ = self
                    .profiles_store
                    .set_active_profile_id(Some(id.to_string()));
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "activated_profile_id": id,
                        "snapshot": lab.response(),
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({"status": "error", "message": format!("Failed to apply profile to audio loop: {e}")}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_profile_archive(
        &self,
        stream: &mut TcpStream,
        id: &str,
    ) -> Result<()> {
        match self.profiles_store.archive(id) {
            Ok(()) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "archived": true,
                        "profile_id": id,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({"status": "error", "message": e.to_string()}),
                )
                .await
            }
        }
    }

    async fn handle_api_delete_profile_by_id(
        &self,
        stream: &mut TcpStream,
        id: &str,
    ) -> Result<()> {
        match self.profiles_store.delete(id) {
            Ok(()) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "deleted": true,
                        "profile_id": id,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({"status": "error", "message": e.to_string()}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_profile_rename(
        &self,
        stream: &mut TcpStream,
        id: &str,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct RenameDto {
            name: String,
        }
        let dto: RenameDto = match serde_json::from_str(body) {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status": "error", "message": format!("Invalid rename payload: {e}")}),
                    )
                    .await;
            }
        };
        match self.profiles_store.rename(id, &dto.name) {
            Ok(profile) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "profile": profile,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({"status": "error", "message": e.to_string()}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_profiles_export(&self, stream: &mut TcpStream) -> Result<()> {
        match self.profiles_store.export_all() {
            Ok(export) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "export": export,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({"status": "error", "message": e.to_string()}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_profiles_import(
        &self,
        stream: &mut TcpStream,
        body: &str,
        identity: &RequestIdentity,
    ) -> Result<()> {
        let export: crate::acoustics::profiles::ProfilesExportDto = match serde_json::from_str(body)
        {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status": "error", "message": format!("Invalid export payload: {e}")}),
                    )
                    .await;
            }
        };
        let is_owner = identity.is_owner();
        match self.profiles_store.import_all(export, is_owner) {
            Ok(imported_count) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "imported_count": imported_count,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({"status": "error", "message": e.to_string()}),
                )
                .await
            }
        }
    }

    async fn handle_api_post_preview(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct PreviewPayload {
            settings: crate::acoustics::AcousticSettingsDto,
            ttl_secs: Option<u64>,
        }
        let payload: PreviewPayload = match serde_json::from_str(body) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status": "error", "message": format!("Invalid preview payload: {e}")}),
                    )
                    .await;
            }
        };

        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({"status": "error", "message": "Audio loop is unavailable"}),
                )
                .await;
        };

        let ttl_secs = payload.ttl_secs.unwrap_or(30).clamp(5, 300);
        match lab
            .apply_candidate_preview(payload.settings, Duration::from_secs(ttl_secs))
            .await
        {
            Ok(_) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "preview": true,
                        "ttl_secs": ttl_secs,
                        "snapshot": lab.response(),
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({"status": "error", "message": format!("Failed to apply preview: {e}")}),
                )
                .await
            }
        }
    }

    async fn handle_api_list_samples(
        &self,
        stream: &mut TcpStream,
        query: Option<&str>,
    ) -> Result<()> {
        let include_archived = query
            .map(|q| q.contains("include_archived=true"))
            .unwrap_or(false);
        let split_filter = query.and_then(|q| {
            q.split('&').find_map(|pair| {
                let mut it = pair.split('=');
                if it.next()? == "split" {
                    match it.next()? {
                        "train" => Some(crate::acoustics::samples::SampleSplit::Train),
                        "calibration" => Some(crate::acoustics::samples::SampleSplit::Calibration),
                        "held_out" => Some(crate::acoustics::samples::SampleSplit::HeldOut),
                        _ => None,
                    }
                } else {
                    None
                }
            })
        });
        let label_filter = query.and_then(|q| {
            q.split('&').find_map(|pair| {
                let mut it = pair.split('=');
                if it.next()? == "label" {
                    match it.next()? {
                        "positive" | "positive_wake_word" => {
                            Some(crate::acoustics::samples::SampleLabel::PositiveWakeWord)
                        }
                        "negative" | "negative_speech" => {
                            Some(crate::acoustics::samples::SampleLabel::NegativeSpeech)
                        }
                        "own_output" => Some(crate::acoustics::samples::SampleLabel::OwnOutput),
                        "ambient" | "ambient_noise" => {
                            Some(crate::acoustics::samples::SampleLabel::AmbientNoise)
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            })
        });

        let samples = self
            .samples_store
            .list(include_archived, split_filter, label_filter);
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "samples": samples,
            }),
        )
        .await
    }

    async fn handle_api_get_sample(&self, stream: &mut TcpStream, sample_id: &str) -> Result<()> {
        match self.samples_store.get_metadata(sample_id) {
            Some(sample) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "sample": sample,
                    }),
                )
                .await
            }
            None => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Sample '{sample_id}' not found")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_get_sample_wav(
        &self,
        stream: &mut TcpStream,
        sample_id: &str,
    ) -> Result<()> {
        match self.samples_store.get_wav_bytes(sample_id) {
            Ok(bytes) => {
                self.send_response(stream, "200 OK", "audio/wav", &bytes)
                    .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({
                        "status": "error",
                        "message": e.to_string()
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_get_sample_spectrogram(
        &self,
        stream: &mut TcpStream,
        sample_id: &str,
    ) -> Result<()> {
        match self.samples_store.get_spectrogram(sample_id) {
            Ok(spectrogram) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "sample_id": sample_id,
                        "spectrogram": spectrogram
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({
                        "status": "error",
                        "message": e.to_string()
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_create_sample(
        &self,
        stream: &mut TcpStream,
        body: &str,
        content_type: &str,
        raw_body: &[u8],
        query: Option<&str>,
    ) -> Result<()> {
        let (dto, raw_wav_bytes, auto_convert) = if content_type.starts_with("application/json") {
            let req: crate::acoustics::samples::CreateSampleDto = match serde_json::from_str(body) {
                Ok(v) => v,
                Err(e) => {
                    return self
                        .send_json_status(
                            stream,
                            "400 Bad Request",
                            &serde_json::json!({"status": "error", "message": format!("Invalid JSON sample dto: {e}")}),
                        )
                        .await;
                }
            };
            let b64 = match &req.wav_base64 {
                Some(b) if !b.is_empty() => b.as_str(),
                _ => {
                    return self
                        .send_json_status(
                            stream,
                            "400 Bad Request",
                            &serde_json::json!({"status": "error", "message": "Missing wav_base64 audio data in JSON"}),
                        )
                        .await;
                }
            };
            use base64::Engine;
            let bytes = match base64::prelude::BASE64_STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => {
                    return self
                        .send_json_status(
                            stream,
                            "400 Bad Request",
                            &serde_json::json!({"status": "error", "message": format!("Invalid base64 audio: {e}")}),
                        )
                        .await;
                }
            };
            let auto_conv = req.auto_convert.unwrap_or(false);
            (req, bytes, auto_conv)
        } else if content_type.starts_with("audio/wav")
            || content_type.starts_with("application/octet-stream")
        {
            let name = query
                .and_then(|q| {
                    q.split('&').find_map(|p| {
                        p.strip_prefix("name=")
                            .map(|v| v.replace("%20", " ").replace('+', " "))
                    })
                })
                .unwrap_or_else(|| "Uploaded Sample".into());

            let label = query
                .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("label=")))
                .and_then(|l| match l {
                    "positive" | "positive_wake_word" => {
                        Some(crate::acoustics::samples::SampleLabel::PositiveWakeWord)
                    }
                    "negative" | "negative_speech" => {
                        Some(crate::acoustics::samples::SampleLabel::NegativeSpeech)
                    }
                    "own_output" => Some(crate::acoustics::samples::SampleLabel::OwnOutput),
                    "ambient" | "ambient_noise" => {
                        Some(crate::acoustics::samples::SampleLabel::AmbientNoise)
                    }
                    _ => None,
                })
                .unwrap_or(crate::acoustics::samples::SampleLabel::PositiveWakeWord);

            let split = query
                .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("split=")))
                .and_then(|s| match s {
                    "train" => Some(crate::acoustics::samples::SampleSplit::Train),
                    "calibration" => Some(crate::acoustics::samples::SampleSplit::Calibration),
                    "held_out" => Some(crate::acoustics::samples::SampleSplit::HeldOut),
                    _ => None,
                })
                .unwrap_or(crate::acoustics::samples::SampleSplit::Train);

            let start_ms = query
                .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("start_ms=")))
                .and_then(|v| v.parse::<u32>().ok());
            let end_ms = query
                .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("end_ms=")))
                .and_then(|v| v.parse::<u32>().ok());
            let interval = match (start_ms, end_ms) {
                (Some(s), Some(e)) => Some(crate::acoustics::samples::SampleInterval {
                    start_ms: s,
                    end_ms: e,
                }),
                _ => None,
            };
            let auto_conv = query.is_some_and(|q| {
                q.split('&')
                    .any(|p| p == "auto_convert=true" || p == "auto_convert=1")
            });

            let dto = crate::acoustics::samples::CreateSampleDto {
                name,
                source: Some(crate::acoustics::samples::SampleSource::Upload),
                label,
                split,
                interval,
                notes: None,
                wav_base64: None,
                auto_convert: Some(auto_conv),
            };
            (dto, raw_body.to_vec(), auto_conv)
        } else {
            return self
                .send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({"status": "error", "message": "Expected Content-Type application/json or audio/wav"}),
                )
                .await;
        };

        let wav_bytes = if auto_convert {
            match crate::acoustics::samples::normalize_wav_to_16k_mono_pcm16(&raw_wav_bytes) {
                Ok(converted) => converted,
                Err(e) => {
                    return self
                        .send_json_status(
                            stream,
                            "400 Bad Request",
                            &serde_json::json!({
                                "status": "error",
                                "message": format!("WAV normalization failed: {e}")
                            }),
                        )
                        .await;
                }
            }
        } else {
            raw_wav_bytes
        };

        match self.samples_store.store_sample(dto, &wav_bytes) {
            Ok(meta) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "sample": meta,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({
                        "status": "error",
                        "message": e.to_string()
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_update_sample(
        &self,
        stream: &mut TcpStream,
        sample_id: &str,
        body: &str,
    ) -> Result<()> {
        let dto: crate::acoustics::samples::UpdateSampleDto = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({"status": "error", "message": format!("Invalid update dto: {e}")}),
                    )
                    .await;
            }
        };

        match self.samples_store.update_sample(sample_id, dto) {
            Ok(meta) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "sample": meta,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({
                        "status": "error",
                        "message": e.to_string()
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_archive_sample(
        &self,
        stream: &mut TcpStream,
        sample_id: &str,
    ) -> Result<()> {
        match self.samples_store.archive_sample(sample_id) {
            Ok(meta) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "sample": meta,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({
                        "status": "error",
                        "message": e.to_string()
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_delete_sample(
        &self,
        stream: &mut TcpStream,
        sample_id: &str,
    ) -> Result<()> {
        match self.samples_store.delete_sample(sample_id) {
            Ok(()) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "deleted": sample_id,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({
                        "status": "error",
                        "message": e.to_string()
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_list_jobs(&self, stream: &mut TcpStream) -> Result<()> {
        let snapshot = self.job_runner.snapshot();
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "active_job": snapshot.active_job,
                "history": snapshot.history,
                "aux_supported": snapshot.aux_supported,
            }),
        )
        .await
    }

    async fn handle_api_create_job(
        &self,
        stream: &mut TcpStream,
        body: &str,
        idempotency_key: Option<&str>,
        identity: &RequestIdentity,
    ) -> Result<()> {
        let mut dto: crate::acoustics::jobs::CreateJobDto = match serde_json::from_str(body) {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Invalid create job payload: {e}")
                        }),
                    )
                    .await;
            }
        };
        if let Some(max_pct) = self.auth_manager.max_volume_pct(identity) {
            dto.volume_pct = dto.volume_pct.min(max_pct);
        }

        match self.job_runner.start_job(dto, idempotency_key) {
            Ok(summary) => {
                if let Some(sess) = self.session_manager.get_session(None) {
                    let _ = self
                        .session_manager
                        .record_job(&sess.session_id, &summary.job_id);
                }
                self.send_json_status(
                    stream,
                    "201 Created",
                    &serde_json::json!({
                        "status": "ok",
                        "job": summary,
                    }),
                )
                .await
            }
            Err(err) => self.send_job_error(stream, err).await,
        }
    }

    async fn handle_api_get_job(&self, stream: &mut TcpStream, job_id: &str) -> Result<()> {
        match self.job_runner.get_job(job_id) {
            Some(detail) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "job": detail,
                    }),
                )
                .await
            }
            None => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Job '{job_id}' not found")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_cancel_job(
        &self,
        stream: &mut TcpStream,
        job_id: &str,
        body: &str,
    ) -> Result<()> {
        let reason = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(String::from))
            .unwrap_or_else(|| "Cancelled by client".to_string());

        match self.job_runner.cancel_active(&reason) {
            Ok(summary) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "job": summary,
                    }),
                )
                .await
            }
            Err(crate::acoustics::jobs::JobError::NotFound(_)) => {
                // If it's already in history, return current state
                if let Some(detail) = self.job_runner.get_job(job_id) {
                    self.send_json(
                        stream,
                        &serde_json::json!({
                            "status": "ok",
                            "job": detail.summary,
                        }),
                    )
                    .await
                } else {
                    self.send_json_status(
                        stream,
                        "404 Not Found",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Job '{job_id}' not found")
                        }),
                    )
                    .await
                }
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({
                        "status": "error",
                        "message": e.to_string(),
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_get_job_events(&self, stream: &mut TcpStream, job_id: &str) -> Result<()> {
        if let Some(events) = self.job_runner.job_events(job_id) {
            self.send_json(
                stream,
                &serde_json::json!({
                    "status": "ok",
                    "job_id": job_id,
                    "events": events,
                }),
            )
            .await
        } else if let Some(job) = self.job_runner.get_job(job_id) {
            self.send_json(
                stream,
                &serde_json::json!({
                    "status": "ok",
                    "job_id": job_id,
                    "events": job.events,
                }),
            )
            .await
        } else {
            self.send_json_status(
                stream,
                "404 Not Found",
                &serde_json::json!({
                    "status": "error",
                    "message": format!("Job '{job_id}' not found")
                }),
            )
            .await
        }
    }

    async fn handle_api_get_job_report(&self, stream: &mut TcpStream, job_id: &str) -> Result<()> {
        match self.job_runner.get_report(job_id) {
            Some(report) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "report": report,
                    }),
                )
                .await
            }
            None => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Report for job '{job_id}' not found")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_evaluate(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct EvaluateReq {
            #[serde(default)]
            settings: Option<crate::acoustics::AcousticSettingsDto>,
            #[serde(default)]
            split: Option<String>,
            #[serde(default)]
            sample_id: Option<String>,
        }

        let req: EvaluateReq = serde_json::from_str(body).unwrap_or(EvaluateReq {
            settings: None,
            split: None,
            sample_id: None,
        });

        let split = match req.split.as_deref() {
            Some("train") => crate::acoustics::samples::SampleSplit::Train,
            Some("held_out") | Some("heldout") => crate::acoustics::samples::SampleSplit::HeldOut,
            _ => crate::acoustics::samples::SampleSplit::Calibration,
        };

        let baseline_config = if let Some(ref lab) = self.acoustic_lab {
            lab.snapshot().saved
        } else {
            crate::config::WakewordCalibrationConfig::default()
        };

        let config = if let Some(dto) = req.settings {
            dto.into()
        } else {
            baseline_config
        };

        let evaluator = crate::acoustics::evaluator::AcousticEvaluator::new(
            self.samples_store.clone(),
            Some(self.wakeword_model_path.clone()),
        );

        let eval_res = if let Some(ref sid) = req.sample_id {
            evaluator.evaluate_single_sample(&config, sid)
        } else {
            evaluator.evaluate_candidate(&config, split)
        };

        match eval_res {
            Ok(report) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "evaluation": report,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Evaluation failed: {e}")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_search(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct SearchReq {
            #[serde(default)]
            baseline: Option<crate::acoustics::AcousticSettingsDto>,
            #[serde(default)]
            split: Option<String>,
            #[serde(default)]
            max_trials: Option<usize>,
        }

        let req: SearchReq = serde_json::from_str(body).unwrap_or(SearchReq {
            baseline: None,
            split: None,
            max_trials: None,
        });

        let split = match req.split.as_deref() {
            Some("train") => crate::acoustics::samples::SampleSplit::Train,
            Some("held_out") | Some("heldout") => crate::acoustics::samples::SampleSplit::HeldOut,
            _ => crate::acoustics::samples::SampleSplit::Calibration,
        };

        let baseline_config = if let Some(dto) = req.baseline {
            dto.into()
        } else if let Some(ref lab) = self.acoustic_lab {
            lab.snapshot().saved
        } else {
            crate::config::WakewordCalibrationConfig::default()
        };

        let max_trials = req.max_trials.unwrap_or(10);

        let evaluator = Arc::new(crate::acoustics::evaluator::AcousticEvaluator::new(
            self.samples_store.clone(),
            Some(self.wakeword_model_path.clone()),
        ));
        let searcher = crate::acoustics::evaluator::AcousticSearcher::new(evaluator);

        match searcher.search(&baseline_config, split, max_trials) {
            Ok(report) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "search": report,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Search failed: {e}")
                    }),
                )
                .await
            }
        }
    }

    async fn send_job_error(
        &self,
        stream: &mut TcpStream,
        err: crate::acoustics::jobs::JobError,
    ) -> Result<()> {
        let (status, code, message) = match err {
            crate::acoustics::jobs::JobError::Conflict(e) => ("409 Conflict", Some("conflict"), e),
            crate::acoustics::jobs::JobError::Unsupported(e) => {
                ("422 Unprocessable Entity", Some("unsupported"), e)
            }
            crate::acoustics::jobs::JobError::Validation(e) => {
                ("400 Bad Request", Some("validation"), e)
            }
            crate::acoustics::jobs::JobError::NotFound(e) => {
                ("404 Not Found", Some("not_found"), e)
            }
            e => ("500 Internal Server Error", None, e.to_string()),
        };
        let mut payload = serde_json::json!({
            "status": "error",
            "message": message,
        });
        if let Some(c) = code {
            payload["code"] = serde_json::json!(c);
        }
        self.send_json_status(stream, status, &payload).await
    }

    fn max_level_for_soloist_cap(cap: u8) -> u8 {
        (0u8..=10)
            .rev()
            .find(|&l| volume::soloist_percent_for_level(l) <= cap)
            .unwrap_or(0)
    }

    fn clamp_calibrated_volume(
        requested_level: Option<u8>,
        requested_pct: Option<u8>,
        lease_cap: Option<u8>,
    ) -> (u8, u8, bool) {
        match (requested_level, requested_pct) {
            (Some(lvl), None) => {
                let req_lvl = lvl.min(10);
                let req_pct = volume::soloist_percent_for_level(req_lvl);
                let eff_lvl = match lease_cap {
                    Some(cap) => req_lvl.min(Self::max_level_for_soloist_cap(cap)),
                    None => req_lvl,
                };
                let eff_pct = volume::soloist_percent_for_level(eff_lvl);
                let clamped = eff_lvl < req_lvl || lease_cap.is_some_and(|cap| req_pct > cap);
                (eff_lvl, eff_pct, clamped)
            }
            (opt_lvl, Some(pct)) => {
                let req_pct = pct.min(100);
                let eff_pct = lease_cap.map_or(req_pct, |cap| req_pct.min(cap));
                let base_lvl = opt_lvl
                    .map_or_else(|| volume::level_for_soloist_percent(eff_pct), |l| l.min(10));
                let mut eff_lvl = match lease_cap {
                    Some(cap) => base_lvl.min(Self::max_level_for_soloist_cap(cap)),
                    None => base_lvl,
                };
                if eff_pct == 0 {
                    eff_lvl = 0;
                }
                let clamped = eff_pct < req_pct || opt_lvl.is_some_and(|l| eff_lvl < l.min(10));
                (eff_lvl, eff_pct, clamped)
            }
            (None, None) => {
                let cur = volume::current_level();
                (cur, volume::soloist_percent_for_level(cur), false)
            }
        }
    }

    fn soloist_state_name(state: &PlaybackState) -> String {
        match state {
            PlaybackState::Playing => "playing".to_string(),
            PlaybackState::Paused => "paused".to_string(),
            PlaybackState::Stopped => "stopped".to_string(),
            PlaybackState::Unknown(other) => other.clone(),
        }
    }

    async fn handle_api_post_cue(
        &self,
        stream: &mut TcpStream,
        body: &str,
        identity: &RequestIdentity,
    ) -> Result<()> {
        let req: crate::acoustics::jobs::CueRequestDto = if body.trim().is_empty() {
            serde_json::from_str("{}").unwrap()
        } else {
            match serde_json::from_str(body) {
                Ok(v) => v,
                Err(e) => {
                    return self
                        .send_json_status(
                            stream,
                            "400 Bad Request",
                            &serde_json::json!({
                                "status": "error",
                                "message": format!("Invalid cue request JSON: {e}"),
                            }),
                        )
                        .await;
                }
            }
        };

        let lease_cap = self.auth_manager.max_volume_pct(identity);
        match self.job_runner.play_cue(&req, lease_cap).await {
            Ok(result) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "cue": result,
                    }),
                )
                .await
            }
            Err(err) => self.send_job_error(stream, err).await,
        }
    }

    async fn handle_api_get_acoustics_volume(
        &self,
        stream: &mut TcpStream,
        identity: &RequestIdentity,
    ) -> Result<()> {
        let level = volume::current_level();
        let soloist_pct = volume::soloist_percent_for_level(level);
        let soloist_actual_pct = if let Some(ref soloist) = self.soloist_client {
            tokio::time::timeout(Duration::from_secs(2), soloist.get_status())
                .await
                .ok()
                .and_then(|r| r.ok())
                .map(|s| s.volume)
        } else {
            None
        };

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "level": level,
                "soloist_pct": soloist_pct,
                "soloist_actual_pct": soloist_actual_pct,
                "soloist_available": self.soloist_client.is_some(),
                "ducked": self.ducker.is_ducked(),
                "max_volume_pct": self.auth_manager.max_volume_pct(identity),
            }),
        )
        .await
    }

    async fn handle_api_post_acoustics_volume(
        &self,
        stream: &mut TcpStream,
        body: &str,
        identity: &RequestIdentity,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct AcousticsVolumeReq {
            #[serde(default)]
            level: Option<u8>,
            #[serde(default)]
            soloist_pct: Option<u8>,
            #[serde(default)]
            volume_pct: Option<u8>,
        }

        let req: AcousticsVolumeReq = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Invalid volume JSON: {e}"),
                        }),
                    )
                    .await;
            }
        };

        let req_pct = req.soloist_pct.or(req.volume_pct);
        if req.level.is_none() && req_pct.is_none() {
            return self
                .send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({
                        "status": "error",
                        "message": "Specify 'level' (0..=10) or 'soloist_pct' / 'volume_pct' (0..=100)",
                    }),
                )
                .await;
        }

        let lease_cap = self.auth_manager.max_volume_pct(identity);
        let (applied_level, applied_pct, clamped) =
            Self::clamp_calibrated_volume(req.level, req_pct, lease_cap);

        self.ducker.cancel();
        let effective_level = volume::set_volume_level(applied_level);

        let mut soloist_updated = false;
        let mut soloist_error: Option<String> = None;
        if let Some(ref soloist) = self.soloist_client {
            match tokio::time::timeout(Duration::from_secs(2), soloist.set_volume(applied_pct))
                .await
            {
                Ok(Ok(())) => {
                    soloist_updated = true;
                }
                Ok(Err(e)) => {
                    warn!(err = %e, "Acoustics volume: failed to update Soloist volume");
                    soloist_error = Some(e.to_string());
                }
                Err(_) => {
                    warn!("Acoustics volume: Soloist set_volume timed out");
                    soloist_error = Some("Soloist set_volume timed out".to_string());
                }
            }
        }

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "level": effective_level,
                "soloist_pct": applied_pct,
                "clamped": clamped,
                "max_volume_pct": lease_cap,
                "soloist_updated": soloist_updated,
                "soloist_available": self.soloist_client.is_some(),
                "soloist_error": soloist_error,
            }),
        )
        .await
    }

    async fn handle_api_get_reference_playback(&self, stream: &mut TcpStream) -> Result<()> {
        let Some(ref soloist) = self.soloist_client else {
            return self
                .send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "available": false,
                        "state": "unavailable",
                        "is_playing": false,
                        "is_active": false,
                        "volume": null,
                        "position_ms": 0,
                        "duration_ms": 0,
                        "track": null,
                    }),
                )
                .await;
        };

        match tokio::time::timeout(Duration::from_secs(2), soloist.get_status()).await {
            Ok(Ok(st)) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "available": true,
                        "state": Self::soloist_state_name(&st.state),
                        "is_playing": st.is_actually_playing(),
                        "is_active": st.is_active,
                        "volume": st.volume,
                        "position_ms": st.position_ms,
                        "duration_ms": st.duration_ms,
                        "track": st.track,
                    }),
                )
                .await
            }
            Ok(Err(e)) => {
                self.send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({
                        "status": "error",
                        "available": false,
                        "message": format!("Soloist status unavailable: {e}"),
                    }),
                )
                .await
            }
            Err(_) => {
                self.send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({
                        "status": "error",
                        "available": false,
                        "message": "Soloist status query timed out",
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_post_reference_playback(
        &self,
        stream: &mut TcpStream,
        body: &str,
        identity: &RequestIdentity,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct RefPlaybackReq {
            action: String,
            #[serde(default)]
            uri: Option<String>,
            #[serde(default)]
            volume_pct: Option<u8>,
        }

        let req: RefPlaybackReq = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Invalid reference_playback JSON: {e}"),
                        }),
                    )
                    .await;
            }
        };

        let action = req.action.trim().to_ascii_lowercase();
        if !matches!(
            action.as_str(),
            "play" | "resume" | "pause" | "next" | "prev" | "previous" | "volume" | "status"
        ) {
            return self
                .send_json_status(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Unsupported action '{}'; expected play|resume|pause|next|prev|volume|status", req.action),
                    }),
                )
                .await;
        }

        if action == "status" {
            return self.handle_api_get_reference_playback(stream).await;
        }

        let Some(ref soloist) = self.soloist_client else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({
                        "status": "error",
                        "available": false,
                        "message": "Soloist client is not configured on this host",
                    }),
                )
                .await;
        };

        let lease_cap = self.auth_manager.max_volume_pct(identity);
        let mut clamped = false;
        let mut applied_pct = req.volume_pct.map(|p| {
            let clamped_val = lease_cap.map_or(p.min(100), |cap| p.min(100).min(cap));
            if clamped_val < p.min(100) {
                clamped = true;
            }
            clamped_val
        });

        if action == "volume" {
            let target_pct = match applied_pct {
                Some(v) => v,
                None => {
                    return self
                        .send_json_status(
                            stream,
                            "400 Bad Request",
                            &serde_json::json!({
                                "status": "error",
                                "message": "action='volume' requires 'volume_pct' (0..=100)",
                            }),
                        )
                        .await;
                }
            };
            self.ducker.cancel();
            if let Err(e) = soloist.set_volume(target_pct).await {
                return self
                    .send_json_status(
                        stream,
                        "503 Service Unavailable",
                        &serde_json::json!({
                            "status": "error",
                            "available": true,
                            "message": format!("Soloist set_volume failed: {e}"),
                        }),
                    )
                    .await;
            }
        } else {
            if let Some(target_pct) = applied_pct {
                self.ducker.cancel();
                let _ = soloist.set_volume(target_pct).await;
            } else if matches!(action.as_str(), "play" | "resume") {
                if let Some(cap) = lease_cap {
                    if let Ok(Ok(cur)) =
                        tokio::time::timeout(Duration::from_secs(2), soloist.get_status()).await
                    {
                        if cur.volume > cap {
                            clamped = true;
                            applied_pct = Some(cap);
                            self.ducker.cancel();
                            let _ = soloist.set_volume(cap).await;
                        }
                    }
                }
            }

            let op_res = match action.as_str() {
                "play" | "resume" => {
                    ensure_local_device_active(soloist, &self.spotify_client, false).await;
                    if let Some(ref uri) = req.uri {
                        soloist.play_uri(uri).await
                    } else {
                        soloist.resume().await
                    }
                }
                "pause" => soloist.pause().await,
                "next" => {
                    ensure_local_device_active(soloist, &self.spotify_client, false).await;
                    soloist.next().await
                }
                "prev" | "previous" => {
                    ensure_local_device_active(soloist, &self.spotify_client, false).await;
                    soloist.previous().await
                }
                _ => Ok(()),
            };

            if let Err(e) = op_res {
                return self
                    .send_json_status(
                        stream,
                        "503 Service Unavailable",
                        &serde_json::json!({
                            "status": "error",
                            "available": true,
                            "message": format!("Soloist command '{action}' failed: {e}"),
                        }),
                    )
                    .await;
            }
        }

        let st_opt = tokio::time::timeout(Duration::from_secs(2), soloist.get_status())
            .await
            .ok()
            .and_then(|r| r.ok());

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "action": action,
                "available": true,
                "clamped": clamped,
                "applied_volume_pct": applied_pct.or_else(|| st_opt.as_ref().map(|s| s.volume)),
                "max_volume_pct": lease_cap,
                "state": st_opt
                    .as_ref()
                    .map(|s| Self::soloist_state_name(&s.state))
                    .unwrap_or_else(|| "unknown".to_string()),
                "is_playing": st_opt.as_ref().is_some_and(|s| s.is_actually_playing()),
                "is_active": st_opt.as_ref().is_some_and(|s| s.is_active),
                "volume": st_opt.as_ref().map(|s| s.volume),
                "position_ms": st_opt.as_ref().map(|s| s.position_ms).unwrap_or(0),
                "duration_ms": st_opt.as_ref().map(|s| s.duration_ms).unwrap_or(0),
                "track": st_opt.and_then(|s| s.track),
            }),
        )
        .await
    }

    async fn handle_api_get_openapi(&self, stream: &mut TcpStream) -> Result<()> {
        let spec = crate::acoustics::openapi::generate_openapi_spec();
        self.send_json(stream, &spec).await
    }

    async fn handle_api_get_session(
        &self,
        stream: &mut TcpStream,
        session_id: Option<&str>,
    ) -> Result<()> {
        let sess = self.session_manager.get_session(session_id);
        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "session": sess,
            }),
        )
        .await
    }

    async fn handle_api_create_session(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        let dto: crate::acoustics::sessions::CreateSessionDto = serde_json::from_str(body)
            .unwrap_or(crate::acoustics::sessions::CreateSessionDto {
                name: None,
                max_duration_secs: None,
                max_jobs: None,
                require_aux: None,
                notes: None,
            });

        // Validation for require_aux: if required but not supported, return 422
        if dto.require_aux.unwrap_or(false) && !self.job_runner.is_aux_supported() {
            return self
                .send_json_status(
                    stream,
                    "422 Unprocessable Entity",
                    &serde_json::json!({
                        "status": "error",
                        "message": "AUX output route is required for this session but is not supported on this device"
                    }),
                )
                .await;
        }

        let (baseline_cfg, baseline_rev) = if let Some(ref lab) = self.acoustic_lab {
            let snap = lab.snapshot();
            (snap.saved, snap.revision)
        } else {
            (crate::config::WakewordCalibrationConfig::default(), 0)
        };

        match self
            .session_manager
            .create_session(baseline_cfg, baseline_rev, dto)
        {
            Ok(sess) => {
                self.send_json_status(
                    stream,
                    "201 Created",
                    &serde_json::json!({
                        "status": "ok",
                        "session": sess,
                    }),
                )
                .await
            }
            Err(crate::acoustics::sessions::SessionError::Conflict(existing_id)) => {
                self.send_json_status(
                    stream,
                    "409 Conflict",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Session '{existing_id}' is already active"),
                        "active_session_id": existing_id,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Failed to create session: {e}")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_session_candidate(
        &self,
        stream: &mut TcpStream,
        session_id: &str,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct CandidatePayload {
            settings: crate::acoustics::AcousticSettingsDto,
        }
        let payload: CandidatePayload = match serde_json::from_str(body) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .send_json_status(
                        stream,
                        "400 Bad Request",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Invalid candidate payload: {e}")
                        }),
                    )
                    .await;
            }
        };

        let Some(lab) = &self.acoustic_lab else {
            return self
                .send_json_status(
                    stream,
                    "503 Service Unavailable",
                    &serde_json::json!({
                        "status": "error",
                        "message": "Audio loop is unavailable"
                    }),
                )
                .await;
        };

        match lab
            .apply_candidate_in_memory(payload.settings.clone())
            .await
        {
            Ok(snap) => {
                let rev = snap.applied_revision.unwrap_or(snap.revision);
                if let Err(e) =
                    self.session_manager
                        .update_candidate(session_id, payload.settings.clone(), rev)
                {
                    return self
                        .send_json_status(
                            stream,
                            "404 Not Found",
                            &serde_json::json!({
                                "status": "error",
                                "message": format!("Session update failed: {e}")
                            }),
                        )
                        .await;
                }
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "applied_revision": rev,
                        "candidate": payload.settings,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Failed to apply candidate: {e}")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_session_restore(
        &self,
        stream: &mut TcpStream,
        session_id: &str,
    ) -> Result<()> {
        let sess = match self.session_manager.get_session(Some(session_id)) {
            Some(s) => s,
            None => {
                return self
                    .send_json_status(
                        stream,
                        "404 Not Found",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Session '{session_id}' not found")
                        }),
                    )
                    .await;
            }
        };

        if let Some(ref lab) = self.acoustic_lab {
            if let Err(e) = lab
                .apply_config_in_memory(sess.baseline_config.clone())
                .await
            {
                return self
                    .send_json_status(
                        stream,
                        "500 Internal Server Error",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Failed to restore baseline: {e}")
                        }),
                    )
                    .await;
            }
        }

        let _ = self.session_manager.restore_baseline(session_id);

        self.send_json(
            stream,
            &serde_json::json!({
                "status": "ok",
                "restored_baseline_revision": sess.baseline_revision,
            }),
        )
        .await
    }

    async fn handle_api_session_confirm_owner(
        &self,
        stream: &mut TcpStream,
        session_id: &str,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct ConfirmPayload {
            notes: Option<String>,
        }
        let payload: ConfirmPayload =
            serde_json::from_str(body).unwrap_or(ConfirmPayload { notes: None });
        match self
            .session_manager
            .confirm_owner(session_id, payload.notes)
        {
            Ok(()) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "owner_confirmed": true,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Confirm owner failed: {e}")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_session_finish(
        &self,
        stream: &mut TcpStream,
        session_id: &str,
        body: &str,
        identity: &RequestIdentity,
    ) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct FinishPayload {
            #[serde(default)]
            owner_confirmed: Option<bool>,
            #[serde(default)]
            save_profile_name: Option<String>,
            #[serde(default)]
            activate_profile: Option<bool>,
            #[serde(default)]
            notes: Option<String>,
        }
        let payload: FinishPayload = serde_json::from_str(body).unwrap_or(FinishPayload {
            owner_confirmed: None,
            save_profile_name: None,
            activate_profile: None,
            notes: None,
        });

        let sess = match self.session_manager.get_session(Some(session_id)) {
            Some(s) => s,
            None => {
                return self
                    .send_json_status(
                        stream,
                        "404 Not Found",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Session '{session_id}' not found")
                        }),
                    )
                    .await;
            }
        };

        let mut saved_profile_id = None;
        if let Some(ref prof_name) = payload.save_profile_name {
            if let Some(ref cand) = sess.active_candidate {
                let is_owner = identity.is_owner() || payload.owner_confirmed.unwrap_or(false);
                let create_dto = crate::acoustics::profiles::CreateProfileDto {
                    id: None,
                    name: prof_name.clone(),
                    notes: Some(format!("Profile from calibration session {}", session_id)),
                    settings: cand.clone(),
                    model_id: None,
                    model_hash: None,
                    audio_pipeline_version: None,
                    conditions: None,
                    status: None,
                };
                match self.profiles_store.create(create_dto, None, is_owner) {
                    Ok(prof) => {
                        saved_profile_id = Some(prof.id.clone());
                        if payload.activate_profile.unwrap_or(false) {
                            if let Some(ref lab) = self.acoustic_lab {
                                let path = self
                                    .config_path
                                    .as_deref()
                                    .unwrap_or_else(|| Path::new("config.toml"));
                                let current_snap = lab.snapshot();
                                let act_payload = crate::acoustics::ApplyPayload {
                                    expected_revision: current_snap.revision,
                                    settings: prof.settings.clone(),
                                    expected_instance_id: None,
                                };
                                if lab.apply(path, act_payload).await.is_ok() {
                                    let _ = self
                                        .profiles_store
                                        .set_active_profile_id(Some(prof.id.clone()));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        return self
                            .send_json_status(
                                stream,
                                "409 Conflict",
                                &serde_json::json!({
                                    "status": "error",
                                    "message": format!("Failed to save profile: {e}")
                                }),
                            )
                            .await;
                    }
                }
            }
        }

        // If not activating a saved profile, restore baseline in lab
        if saved_profile_id.is_none() || !payload.activate_profile.unwrap_or(false) {
            if let Some(ref lab) = self.acoustic_lab {
                let _ = lab
                    .apply_config_in_memory(sess.baseline_config.clone())
                    .await;
            }
        }

        match self.session_manager.finish_session(
            session_id,
            payload.owner_confirmed.unwrap_or(false),
            payload.notes,
        ) {
            Ok(finished) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "session": finished,
                        "saved_profile_id": saved_profile_id,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Failed to finish session: {e}")
                    }),
                )
                .await
            }
        }
    }

    async fn handle_api_session_cancel(
        &self,
        stream: &mut TcpStream,
        session_id: &str,
    ) -> Result<()> {
        let sess = match self.session_manager.get_session(Some(session_id)) {
            Some(s) => s,
            None => {
                return self
                    .send_json_status(
                        stream,
                        "404 Not Found",
                        &serde_json::json!({
                            "status": "error",
                            "message": format!("Session '{session_id}' not found")
                        }),
                    )
                    .await;
            }
        };

        if let Some(ref lab) = self.acoustic_lab {
            let _ = lab.apply_config_in_memory(sess.baseline_config).await;
        }

        match self.session_manager.cancel_session(session_id) {
            Ok(cancelled) => {
                self.send_json(
                    stream,
                    &serde_json::json!({
                        "status": "ok",
                        "session": cancelled,
                    }),
                )
                .await
            }
            Err(e) => {
                self.send_json_status(
                    stream,
                    "500 Internal Server Error",
                    &serde_json::json!({
                        "status": "error",
                        "message": format!("Failed to cancel session: {e}")
                    }),
                )
                .await
            }
        }
    }

    fn resolve_config_path(&self) -> Option<std::path::PathBuf> {
        if let Some(ref path) = self.config_path {
            return Some(path.clone());
        }
        if std::path::Path::new("config.toml").exists() {
            Some(std::path::PathBuf::from("config.toml"))
        } else if std::path::Path::new("/data/opt/mini-assistant/config.toml").exists() {
            Some(std::path::PathBuf::from(
                "/data/opt/mini-assistant/config.toml",
            ))
        } else {
            None
        }
    }

    fn read_current_voice_and_provider(&self) -> (String, String) {
        if let Some(path) = self.resolve_config_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut provider = "gemini".to_string();
                let mut voice = String::new();
                let mut in_assistant = false;
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with('[') && trimmed.ends_with(']') {
                        in_assistant = trimmed == "[assistant]";
                        continue;
                    }
                    if in_assistant {
                        if let Some((k, v)) = trimmed.split_once('=') {
                            let k = k.trim();
                            let clean = v
                                .trim()
                                .trim_matches('"')
                                .trim_matches('\'')
                                .split('#')
                                .next()
                                .unwrap_or("")
                                .trim();
                            if k == "provider" && !clean.is_empty() {
                                provider = clean.to_lowercase();
                            } else if k == "voice" && !clean.is_empty() {
                                voice = clean.to_string();
                            }
                        }
                    }
                }
                if voice.is_empty() {
                    voice = if provider == "openai" {
                        "alloy".to_string()
                    } else {
                        "Callirrhoe".to_string()
                    };
                }
                return (provider, voice);
            }
        }
        ("gemini".to_string(), "Callirrhoe".to_string())
    }

    #[allow(dead_code)]
    fn read_current_voice(&self) -> String {
        self.read_current_voice_and_provider().1
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

    async fn send_not_found(&self, stream: &mut TcpStream) -> Result<()> {
        self.send_response(stream, "404 Not Found", "text/plain", b"Not Found")
            .await
    }

    async fn send_response(
        &self,
        stream: &mut TcpStream,
        status: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<()> {
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache, no-store, must-revalidate\r\nPragma: no-cache\r\nExpires: 0\r\nConnection: close\r\n\r\n",
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

    async fn send_json_with_etag(
        &self,
        stream: &mut TcpStream,
        val: &serde_json::Value,
        etag: &str,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(val)?;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nETag: {etag}\r\nCache-Control: no-cache, no-store, must-revalidate\r\nPragma: no-cache\r\nExpires: 0\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        Ok(())
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
        if !self.wakeword_enabled {
            return "Отключено (Deaf mode)".to_string();
        }
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
            .unwrap_or_else(|| self.read_active_wakeword_name());

        let session = EnrollmentSession::new(kw_name.clone());
        let (phase, phase_name) = EnrollmentSession::phase_for_step(1);
        *self.wakeword_session.write().await = Some(session);

        info!(keyword = %kw_name, "Started interactive wake-word enrollment session");

        let resp = serde_json::json!({
            "status": "ok",
            "keyword_name": kw_name,
            "current_step": 1,
            "target_steps": crate::wakeword::enrollment::TARGET_STEPS,
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

        // Play brief Ready chime and let speaker decay settle before opening mic capture
        let ready_cue = crate::acoustics::jobs::CueRequestDto {
            kind: crate::acoustics::jobs::CueKind::Ready,
            freq_hz: None,
            duration_ms: None,
            volume_dbfs: Some(-14.0),
            volume_pct: Some(45),
        };
        if let Err(e) = self.job_runner.play_cue(&ready_cue, None).await {
            tracing::debug!("Wakeword pre-record cue skipped: {e}");
        } else {
            tokio::time::sleep(Duration::from_millis(160)).await;
        }

        // Visual indicator: switch LED ring to bright green (distinct from idle amber) while recording
        let prev_color = self.current_led_color.read().await.clone();
        set_led_hardware("emerald");

        // Record 3.8 seconds of audio from Mini's hardware microphone (accommodates compound words up to 5 syllables)
        let record_res = recorder.record(Duration::from_millis(3800)).await;

        // Restore LED and play soft completion tick asynchronously
        set_led_hardware(&prev_color);
        let runner = self.job_runner.clone();
        tokio::spawn(async move {
            let ack_cue = crate::acoustics::jobs::CueRequestDto {
                kind: crate::acoustics::jobs::CueKind::Ack,
                freq_hz: None,
                duration_ms: None,
                volume_dbfs: Some(-18.0),
                volume_pct: Some(45),
            };
            let _ = runner.play_cue(&ack_cue, None).await;
        });

        let pcm = match record_res {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "Hardware wakeword capture failed");
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
                tracing::info!(
                    sample_id = sample.id,
                    duration_ms = sample.duration_ms,
                    snr_db = sample.snr,
                    peak_rms = sample.peak_rms,
                    "Wakeword sample accepted"
                );
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
                let raw_rms = crate::acoustics::pcm_rms(
                    &pcm.iter()
                        .flat_map(|s| s.to_le_bytes())
                        .collect::<Vec<u8>>(),
                );
                tracing::warn!(
                    error = %e,
                    raw_samples = pcm.len(),
                    raw_rms,
                    "Wakeword sample rejected"
                );
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

    async fn handle_api_wakeword_compile(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        let mut session_guard = self.wakeword_session.write().await;
        let mut sess = match session_guard.take() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({ "status": "error", "message": "Сессия не активна" });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        // Accept optional updated keyword name from compile payload
        #[derive(serde::Deserialize)]
        struct CompileReq {
            name: Option<String>,
        }
        if let Ok(req) = serde_json::from_str::<CompileReq>(body) {
            if let Some(n) = req
                .name
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                sess.keyword_name = n;
            }
        }

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

                // Synchronize config.toml
                if let Some(ref cfg_path) = self.config_path {
                    let _ = update_config_wakeword(cfg_path, &compile_res.keyword_name);
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

    async fn handle_api_wakeword_rename(&self, stream: &mut TcpStream, body: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct RenameReq {
            name: Option<String>,
        }
        let req: RenameReq = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                let err = serde_json::json!({ "status": "error", "message": format!("Неверный JSON: {e}") });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        let raw_name = match req
            .name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(n) => n,
            None => {
                let err = serde_json::json!({ "status": "error", "message": "Имя слова вызова не может быть пустым" });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        if raw_name.len() > 64 {
            let err = serde_json::json!({ "status": "error", "message": "Имя слова вызова слишком длинное (макс 64 символа)" });
            return self.send_json_status(stream, "400 Bad Request", &err).await;
        }

        // 1. Rename in model file on disk
        if self.wakeword_model_path.exists() {
            if let Some(path_str) = self.wakeword_model_path.to_str() {
                match WakewordRef::load_from_file(path_str) {
                    Ok(mut reference) => {
                        reference.name = raw_name.to_lowercase();
                        match reference.save_to_buffer() {
                            Ok(bytes) => {
                                if let Err(e) = std::fs::write(&self.wakeword_model_path, bytes) {
                                    tracing::warn!(
                                        "Failed to save renamed wakeword model to file: {e}"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to serialize renamed wakeword model: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Could not load model for rename: {e}");
                    }
                }
            }
        }

        // 2. Trigger hot reload to InputGate
        if let Some(ref tx) = self.wakeword_reload_tx {
            let _ = tx.send(self.wakeword_model_path.clone());
            info!(path = ?self.wakeword_model_path, keyword = %raw_name, "Triggered hot-reload of renamed wake-word model");
        }

        // 3. Update config.toml
        if let Some(ref cfg_path) = self.config_path {
            if let Err(e) = update_config_wakeword(cfg_path, &raw_name) {
                tracing::warn!("Failed to update config.toml with new wakeword: {e}");
            }
        }

        // 4. Flash emerald LED
        set_led_hardware("emerald");

        let resp = serde_json::json!({
            "status": "ok",
            "keyword_name": raw_name,
            "message": format!("Слово вызова успешно обновлено на «{}»!", raw_name)
        });
        self.send_json(stream, &resp).await
    }

    async fn handle_api_wakeword_analysis(&self, stream: &mut TcpStream) -> Result<()> {
        let session_guard = self.wakeword_session.read().await;
        let sess = match session_guard.as_ref() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({
                    "status": "error",
                    "message": "Сессия обучения не активна."
                });
                return self.send_json_status(stream, "400 Bad Request", &err).await;
            }
        };

        let report = sess.analyze_signature();
        let resp = serde_json::json!({
            "status": "ok",
            "report": report,
        });
        self.send_json(stream, &resp).await
    }
}

#[cfg(target_os = "linux")]
pub fn set_led_hardware(color: &str) {
    use std::os::fd::AsRawFd;

    let (r, g, b) = match color.to_ascii_lowercase().as_str() {
        "azure" | "blue" => (0x00, 0xa0, 0xff),
        "purple" => (0xff, 0x00, 0x80),
        "emerald" | "green" => (0x00, 0xff, 0x60),
        "amber" | "yellow" => (0xff, 0xa0, 0x00),
        "white" => (0xff, 0xff, 0xff),
        "off" => (0x00, 0x00, 0x00),
        _ => (0x00, 0xa0, 0xff),
    };

    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/i2c-0")
    else {
        fallback_i2cset_led(r, g, b);
        return;
    };

    let fd = file.as_raw_fd();
    unsafe {
        if libc::ioctl(fd, 0x0703 /* I2C_SLAVE */, 0x3c as libc::c_ulong) < 0 {
            warn!(
                "ioctl I2C_SLAVE 0x3c failed: {}, using fallback i2cset",
                std::io::Error::last_os_error()
            );
            fallback_i2cset_led(r, g, b);
            return;
        }
    }

    let write_reg = |reg: u8, val: u8| -> bool {
        let buf = [reg, val];
        let res = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, 2) };
        res == 2
    };

    if r == 0 && g == 0 && b == 0 {
        for reg in 0x01..=0x24 {
            let _ = write_reg(reg, 0x00);
        }
        let _ = write_reg(0x25, 0x00);
        if !write_reg(0x00, 0x00) {
            fallback_i2cset_led(0, 0, 0);
        }
        debug!("Hardware LED ring turned OFF (PWM zeroed, latched, shutdown)");
        return;
    }

    // 1. Enable chip (0x00 -> 0x01: normal operation)
    if !write_reg(0x00, 0x01) {
        warn!("write_reg 0x00 failed, using fallback i2cset");
        fallback_i2cset_led(r, g, b);
        return;
    }
    // 2. Global Control (0x4a -> 0x00: global enable, 0 = normal operation, 1 = shutdown all)
    write_reg(0x4a, 0x00);
    // 3. PWM frequency (0x4b -> 0x01: 22 kHz high-frequency clock)
    write_reg(0x4b, 0x01);
    // 4. Enable all 36 channels (0x26..=0x49 -> 0x01: enable with I_max)
    for reg in 0x26..=0x49 {
        write_reg(reg, 0x01);
    }
    // 5. Set PWM for 12 RGB LEDs (36 channels, BGR order on Station Mini 1)
    for i in 0..12 {
        write_reg(1 + i * 3, b);
        write_reg(2 + i * 3, g);
        write_reg(3 + i * 3, r);
    }
    // 6. Latch update register (0x25 -> 0x00: write 0x00 to latch shadow registers into outputs)
    write_reg(0x25, 0x00);
    debug!(
        "Updated hardware LED ring to color '{}' (r={:02x}, g={:02x}, b={:02x})",
        color, r, g, b
    );
}

#[cfg(target_os = "linux")]
fn fallback_i2cset_led(r: u8, g: u8, b: u8) {
    let bin = if std::path::Path::new("/usr/sbin/i2cset").exists() {
        "/usr/sbin/i2cset"
    } else {
        "i2cset"
    };

    if r == 0 && g == 0 && b == 0 {
        for reg in 0x01..=0x24 {
            let _ = std::process::Command::new(bin)
                .args(["-y", "0", "0x3c", &format!("0x{:02x}", reg), "0x00"])
                .status();
        }
        let _ = std::process::Command::new(bin)
            .args(["-y", "0", "0x3c", "0x25", "0x00"])
            .status();
        let _ = std::process::Command::new(bin)
            .args(["-y", "0", "0x3c", "0x00", "0x00"])
            .status();
        return;
    }

    let _ = std::process::Command::new(bin)
        .args(["-y", "0", "0x3c", "0x00", "0x01"])
        .status();
    let _ = std::process::Command::new(bin)
        .args(["-y", "0", "0x3c", "0x4a", "0x00"])
        .status();
    let _ = std::process::Command::new(bin)
        .args(["-y", "0", "0x3c", "0x4b", "0x01"])
        .status();

    for reg in 0x26..=0x49 {
        let _ = std::process::Command::new(bin)
            .args(["-y", "0", "0x3c", &format!("0x{:02x}", reg), "0x01"])
            .status();
    }

    for i in 0..12 {
        let _ = std::process::Command::new(bin)
            .args([
                "-y",
                "0",
                "0x3c",
                &format!("0x{:02x}", 1 + i * 3),
                &format!("0x{:02x}", b),
            ])
            .status();
        let _ = std::process::Command::new(bin)
            .args([
                "-y",
                "0",
                "0x3c",
                &format!("0x{:02x}", 2 + i * 3),
                &format!("0x{:02x}", g),
            ])
            .status();
        let _ = std::process::Command::new(bin)
            .args([
                "-y",
                "0",
                "0x3c",
                &format!("0x{:02x}", 3 + i * 3),
                &format!("0x{:02x}", r),
            ])
            .status();
    }

    let _ = std::process::Command::new(bin)
        .args(["-y", "0", "0x3c", "0x25", "0x00"])
        .status();
}

#[cfg(not(target_os = "linux"))]
pub fn set_led_hardware(_color: &str) {}

pub fn update_config_voice_and_provider(
    config_path: &Path,
    new_provider: Option<&str>,
    new_voice: Option<&str>,
) -> Result<()> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file at {:?}", config_path))?;
    let mut updated = Vec::new();
    let mut in_assistant_section = false;
    let mut replaced_voice = false;
    let mut replaced_provider = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_assistant_section = trimmed == "[assistant]";
            updated.push(line.to_string());
            continue;
        }

        if in_assistant_section {
            if let Some(np) = new_provider {
                if trimmed.starts_with("provider") && trimmed.contains('=') && !replaced_provider {
                    let indent = line
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .collect::<String>();
                    updated.push(format!("{}provider = \"{}\"", indent, np));
                    replaced_provider = true;
                    continue;
                }
            }
            if let Some(nv) = new_voice {
                if trimmed.starts_with("voice") && trimmed.contains('=') && !replaced_voice {
                    let indent = line
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .collect::<String>();
                    updated.push(format!("{}voice = \"{}\"", indent, nv));
                    replaced_voice = true;
                    continue;
                }
            }
        }

        updated.push(line.to_string());
    }

    if let Some(np) = new_provider {
        if !replaced_provider {
            let mut final_lines = Vec::new();
            for line in updated.drain(..) {
                let is_assistant = line.trim() == "[assistant]";
                final_lines.push(line);
                if is_assistant {
                    final_lines.push(format!("provider = \"{}\"", np));
                }
            }
            updated = final_lines;
        }
    }

    if let Some(nv) = new_voice {
        if !replaced_voice {
            let mut final_lines = Vec::new();
            for line in updated.drain(..) {
                let is_assistant = line.trim() == "[assistant]";
                final_lines.push(line);
                if is_assistant {
                    final_lines.push(format!("voice = \"{}\"", nv));
                }
            }
            updated = final_lines;
        }
    }

    std::fs::write(config_path, updated.join("\n") + "\n")?;
    Ok(())
}

pub fn update_config_voice(config_path: &Path, new_voice: &str) -> Result<()> {
    update_config_voice_and_provider(config_path, None, Some(new_voice))
}

pub fn update_config_led_color(config_path: &Path, new_color: &str) -> Result<()> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file at {:?}", config_path))?;
    let mut updated = Vec::new();
    let mut in_assistant_section = false;
    let mut replaced_color = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_assistant_section = trimmed == "[assistant]";
            updated.push(line.to_string());
            continue;
        }

        if in_assistant_section
            && trimmed.starts_with("led_color")
            && trimmed.contains('=')
            && !replaced_color
        {
            let indent = line
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>();
            updated.push(format!("{}led_color = \"{}\"", indent, new_color));
            replaced_color = true;
            continue;
        }

        updated.push(line.to_string());
    }

    if !replaced_color {
        let mut final_lines = Vec::new();
        for line in updated.drain(..) {
            let is_assistant = line.trim() == "[assistant]";
            final_lines.push(line);
            if is_assistant {
                final_lines.push(format!("led_color = \"{}\"", new_color));
            }
        }
        updated = final_lines;
    }

    std::fs::write(config_path, updated.join("\n") + "\n")?;
    Ok(())
}

pub fn update_config_wakeword(config_path: &Path, new_name: &str) -> Result<()> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file at {:?}", config_path))?;
    let mut updated = Vec::new();
    let mut in_assistant_section = false;
    let mut replaced_name = false;
    let mut replaced_wake_words = false;

    let clean_name = new_name.trim();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_assistant_section = trimmed == "[assistant]";
            updated.push(line.to_string());
            continue;
        }

        if in_assistant_section {
            if trimmed.starts_with("name") && trimmed.contains('=') && !replaced_name {
                let indent = line
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect::<String>();
                updated.push(format!("{}name = \"{}\"", indent, clean_name));
                replaced_name = true;
                continue;
            }
            if trimmed.starts_with("wake_words") && trimmed.contains('=') && !replaced_wake_words {
                let indent = line
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect::<String>();
                updated.push(format!(
                    "{}wake_words = [\"{}\"]",
                    indent,
                    clean_name.to_lowercase()
                ));
                replaced_wake_words = true;
                continue;
            }
        }

        updated.push(line.to_string());
    }

    if !replaced_name || !replaced_wake_words {
        let mut final_lines = Vec::new();
        for line in updated.drain(..) {
            let is_assistant = line.trim() == "[assistant]";
            final_lines.push(line);
            if is_assistant {
                if !replaced_name {
                    final_lines.push(format!("name = \"{}\"", clean_name));
                }
                if !replaced_wake_words {
                    final_lines.push(format!("wake_words = [\"{}\"]", clean_name.to_lowercase()));
                }
            }
        }
        updated = final_lines;
    }

    std::fs::write(config_path, updated.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
pub fn update_config_calibration(
    config_path: &Path,
    cal: &crate::config::WakewordCalibrationConfig,
) -> Result<()> {
    crate::acoustics::save_settings(config_path, cal)
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
    fn test_update_config_voice_and_provider() {
        let temp_dir = std::env::temp_dir();
        let cfg_file = temp_dir.join(format!("test_config_prov_{}.toml", uuid::Uuid::new_v4()));
        let initial_toml = r#"
[assistant]
name = "Штучка"
provider = "gemini"
voice = "Callirrhoe"

[audio]
capture_device = "hw:0,2"
"#;
        std::fs::write(&cfg_file, initial_toml).unwrap();

        update_config_voice_and_provider(&cfg_file, Some("openai"), Some("alloy")).unwrap();
        let updated = std::fs::read_to_string(&cfg_file).unwrap();
        assert!(updated.contains("provider = \"openai\""));
        assert!(updated.contains("voice = \"alloy\""));
        assert!(!updated.contains("provider = \"gemini\""));
        assert!(!updated.contains("voice = \"Callirrhoe\""));

        std::fs::remove_file(&cfg_file).unwrap();
    }

    #[test]
    fn test_update_config_led_color() {
        let temp_dir = std::env::temp_dir();
        let cfg_file = temp_dir.join(format!("test_config_led_{}.toml", uuid::Uuid::new_v4()));
        let initial_toml = r#"
[assistant]
name = "Штучка"
led_color = "azure"

[audio]
capture_device = "hw:0,2"
"#;
        std::fs::write(&cfg_file, initial_toml).unwrap();

        update_config_led_color(&cfg_file, "purple").unwrap();
        let updated = std::fs::read_to_string(&cfg_file).unwrap();
        assert!(updated.contains("led_color = \"purple\""));
        assert!(!updated.contains("led_color = \"azure\""));
        assert!(updated.contains("capture_device = \"hw:0,2\""));

        std::fs::remove_file(&cfg_file).unwrap();
    }

    #[test]
    fn test_update_config_wakeword() {
        let temp_dir = std::env::temp_dir();
        let cfg_file = temp_dir.join(format!("test_config_ww_{}.toml", uuid::Uuid::new_v4()));
        let initial_toml = r#"
[assistant]
name = "Штучка"
wake_words = ["штучка", "штучка-интеллект"]

[audio]
capture_device = "hw:0,2"
"#;
        std::fs::write(&cfg_file, initial_toml).unwrap();

        update_config_wakeword(&cfg_file, "Джарвис").unwrap();
        let updated = std::fs::read_to_string(&cfg_file).unwrap();
        assert!(updated.contains("name = \"Джарвис\""));
        assert!(updated.contains("wake_words = [\"джарвис\"]"));
        assert!(updated.contains("capture_device = \"hw:0,2\""));

        std::fs::remove_file(&cfg_file).unwrap();
    }

    #[test]
    fn test_update_config_calibration() {
        let temp_dir = std::env::temp_dir();
        let cfg_file = temp_dir.join(format!("test_config_cal_{}.toml", uuid::Uuid::new_v4()));
        let initial_toml = r#"
[assistant]
name = "Штучка"
wake_words = ["штучка"]

[audio]
capture_device = "hw:0,2"
"#;
        std::fs::write(&cfg_file, initial_toml).unwrap();

        let cal = crate::config::WakewordCalibrationConfig {
            threshold: 0.42,
            avg_threshold: 0.22,
            min_scores: 5,
            score_mode: "avg".to_string(),
            eager: true,
            vad_threshold: 50.0,
            gain_normalizer: true,
            band_pass: true,
            ..Default::default()
        };

        update_config_calibration(&cfg_file, &cal).unwrap();
        let updated = std::fs::read_to_string(&cfg_file).unwrap();
        let parsed: toml::Value = toml::from_str(&updated).unwrap();
        let restored: crate::config::WakewordCalibrationConfig =
            parsed["wakeword"].clone().try_into().unwrap();
        assert_eq!(restored, cal);
        assert_eq!(parsed["audio"]["capture_device"].as_str(), Some("hw:0,2"));

        std::fs::remove_file(&cfg_file).unwrap();
    }

    #[test]
    fn test_url_encoding_roundtrip() {
        let orig = "https://example.com/api?query=штучка & test=1";
        let encoded = urlencoding::encode(orig);
        let decoded = urlencoding::decode(&encoded);
        assert_eq!(orig, decoded);
    }

    #[tokio::test]
    async fn test_http_auth_and_calibration_lifecycle() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (lab, mut rx) =
            crate::acoustics::AcousticLab::new(crate::config::WakewordCalibrationConfig::default());
        let metrics = crate::acoustics::RuntimeMetrics {
            ready: true,
            ..Default::default()
        };
        lab.ready(crate::config::WakewordCalibrationConfig::default(), metrics);
        let lab_mock = lab.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                lab_mock.acknowledge(req, None);
            }
        });
        let ducker = Arc::new(crate::spotify::ducking::MusicDucker::new());
        let auth_manager = Arc::new(crate::web::auth::AuthManager::new(Some(
            "test-owner-token".to_string(),
        )));
        let server = Arc::new(
            ControlCenterServer::new(
                None,
                "client_id".to_string(),
                "http://127.0.0.1/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                ducker,
            )
            .with_acoustic_lab(lab.clone())
            .with_auth_manager(auth_manager),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut buf)).await
                {
                    Ok(Ok(0)) | Err(_) => break,
                    Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
                    Ok(Err(_)) => break,
                }
            }
            let res_str = String::from_utf8_lossy(&response).to_string();
            let status_line = res_str.lines().next().unwrap_or("");
            let status_code: u16 = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let json_body = res_str
                .split_once("\r\n\r\n")
                .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b.trim()).ok())
                .unwrap_or(serde_json::Value::Null);
            (status_code, json_body)
        }

        // 1. Issue calibration token as owner
        let issue_body = serde_json::json!({
            "identity_id": "test-agent",
            "ttl_secs": 3600,
            "scopes": ["acoustics_read", "acoustics_write", "profiles", "samples", "jobs"],
            "lease": {
                "baseline_revision": 1,
                "max_duration_secs": 300,
                "max_volume_pct": 50,
                "max_runs": 5
            }
        })
        .to_string();

        let req = format!(
            "POST /api/v1/acoustics/tokens HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer test-owner-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr,
            issue_body.len(),
            issue_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200, "Failed to issue token: {:?}", resp);
        let token = resp["token"].as_str().unwrap().to_string();
        let lease_id = resp["lease"]["lease_id"].as_str().unwrap().to_string();

        // 2. Calibration token can access acoustic routes
        let req = format!(
            "GET /api/v1/acoustics HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            local_addr, token
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["status"], "ok");

        let req = format!(
            "GET /api/v1/acoustics/events HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            local_addr, token
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["status"], "ok");

        let ts_body = serde_json::json!({"active": true}).to_string();
        let req = format!(
            "POST /api/v1/acoustics/test_session HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr,
            token,
            ts_body.len(),
            ts_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["test_session"], true);
        assert!(lab.snapshot().test_session);

        // 3. Calibration token is forbidden (403) from administrative routes
        let forbidden_routes = [
            "/api/v1/system/restart",
            "/api/v1/system/reboot",
            "/api/v1/voice",
            "/api/v1/volume",
            "/api/v1/led",
            "/api/spotify/play",
            "/api/spotify/activate",
            "/api/v1/wakeword/session/start",
            "/api/v1/acoustics/tokens",
        ];
        for route in forbidden_routes {
            let dummy = "{}";
            let req = format!(
                "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                route,
                local_addr,
                token,
                dummy.len(),
                dummy
            );
            let (code, resp) = send_req(local_addr, &req).await;
            assert_eq!(
                code, 403,
                "Route {} should return 403 for calibration token, got {code} ({:?})",
                route, resp
            );
            assert_eq!(resp["status"], "error");
        }

        // 4. Anonymous external client without authorization header is 401
        let req = format!(
            "GET /api/v1/acoustics/events HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            local_addr
        );
        let (code, _) = send_req(local_addr, &req).await;
        assert_eq!(code, 401);

        let req = format!(
            "POST /api/v1/system/restart HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            local_addr
        );
        let (code, _) = send_req(local_addr, &req).await;
        assert_eq!(code, 401);

        // 5. Empty bearer token is 401
        let req = format!(
            "GET /api/v1/acoustics/events HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer \r\nConnection: close\r\n\r\n",
            local_addr
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 401);
        assert_eq!(
            resp["message"],
            "Authorization header present but bearer token is missing or empty"
        );

        // 6. Revoke lease stops test session and revokes access
        let revoke_body = serde_json::json!({"lease_id": lease_id}).to_string();
        let req = format!(
            "POST /api/v1/acoustics/lease/revoke HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer test-owner-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr,
            revoke_body.len(),
            revoke_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["lease_revoked"], true);

        // Test session was automatically stopped on lease revocation
        assert!(!lab.snapshot().test_session);

        // Subsequent call with token fails with 403 LeaseRevoked
        let req = format!(
            "POST /api/v1/acoustics/test_session HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr,
            token,
            ts_body.len(),
            ts_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 403);
        assert_eq!(
            resp["message"],
            "Calibration lease has been revoked or expired"
        );

        // Cleanup
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_http_spotify_activate_and_status_routing() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir = std::env::temp_dir().join(format!("http_sp_route_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state_file = temp_dir.join("state.txt");
        let ctl_path = temp_dir.join("soloist-ctl");
        std::fs::write(&state_file, "playing false").unwrap();

        let script = format!(
            r#"#!/bin/sh
STATE_FILE="{state}"
read CUR_STATUS CUR_ACTIVE < "$STATE_FILE"
case "$1" in
  now)
    echo "{{\"type\":\"playback_state\",\"status\":\"$CUR_STATUS\",\"volume\":42,\"is_active\":$CUR_ACTIVE,\"position\":{{\"position_ms\":1000,\"speed\":0}},\"item\":{{\"uri\":\"spotify:track:1\",\"playback\":{{\"duration_ms\":180000}}}}}}"
    ;;
  activate)
    echo "$CUR_STATUS true" > "$STATE_FILE"
    ;;
  play)
    echo "playing $CUR_ACTIVE" > "$STATE_FILE"
    ;;
esac
"#,
            state = state_file.display()
        );
        std::fs::write(&ctl_path, script).unwrap();
        let mut perms = std::fs::metadata(&ctl_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&ctl_path, perms).unwrap();

        let ducker = Arc::new(crate::spotify::ducking::MusicDucker::new());
        let auth_manager = Arc::new(crate::web::auth::AuthManager::new(Some(
            "owner-secret".to_string(),
        )));
        let soloist = Arc::new(SoloistClient::new(&ctl_path, "127.0.0.1:9090"));

        let server = Arc::new(
            ControlCenterServer::new(
                None,
                "test-client".to_string(),
                "http://127.0.0.1:8080/spotify/callback".to_string(),
                Arc::new(RwLock::new(None)),
                Some(soloist),
                ducker,
            )
            .with_auth_manager(auth_manager),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server_task = tokio::spawn(server.run(listener, shutdown_rx));

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(raw.as_bytes()).await.unwrap();
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.unwrap();
            let text = String::from_utf8_lossy(&resp);
            let status_code = text
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("{}");
            (status_code, serde_json::from_str(body).unwrap_or_default())
        }

        // Before activation, MacBook is playing externally (`is_active: false`), so Mini reports not playing locally
        let (code, st_before) = send_req(
            local_addr,
            &format!(
                "GET /api/v1/status HTTP/1.1\r\nHost: {local_addr}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(st_before["spotify"]["reachable"], true);
        assert_eq!(st_before["spotify"]["is_active"], false);
        assert_eq!(st_before["spotify"]["is_playing"], false);

        // Owner activates GPT Mini
        let (code, act_resp) = send_req(
            local_addr,
            &format!(
                "POST /api/spotify/activate HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer owner-secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(act_resp["status"], "ok");
        assert_eq!(act_resp["is_active"], true);

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_http_profiles_lifecycle_and_candidate_preview() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("http_prof_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let profiles_store =
            Arc::new(crate::acoustics::profiles::ProfilesStore::new(&temp_dir).unwrap());
        let (lab, mut rx) =
            crate::acoustics::AcousticLab::new(crate::config::WakewordCalibrationConfig::default());
        let metrics = crate::acoustics::RuntimeMetrics {
            ready: true,
            ..Default::default()
        };
        lab.ready(crate::config::WakewordCalibrationConfig::default(), metrics);
        let lab_mock = lab.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                lab_mock.acknowledge(req, None);
            }
        });
        let ducker = Arc::new(crate::spotify::ducking::MusicDucker::new());
        let auth_manager = Arc::new(crate::web::auth::AuthManager::new(Some(
            "owner-secret-123".to_string(),
        )));

        let cfg_file = temp_dir.join("config.toml");
        let initial_toml = r#"
[assistant]
name = "Штучка"
wake_words = ["штучка"]

[audio]
capture_device = "hw:0,2"
"#;
        std::fs::write(&cfg_file, initial_toml).unwrap();

        let server = Arc::new(
            ControlCenterServer::new(
                Some(cfg_file.clone()),
                "client_id".to_string(),
                "http://127.0.0.1/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                ducker,
            )
            .with_acoustic_lab(lab.clone())
            .with_auth_manager(auth_manager.clone())
            .with_profiles_store(profiles_store.clone()),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut buf)).await
                {
                    Ok(Ok(0)) | Err(_) => break,
                    Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
                    Ok(Err(_)) => break,
                }
            }
            let res_str = String::from_utf8_lossy(&response).to_string();
            let status_line = res_str.lines().next().unwrap_or("");
            let status_code: u16 = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let json_body = res_str
                .split_once("\r\n\r\n")
                .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b.trim()).ok())
                .unwrap_or(serde_json::Value::Null);
            (status_code, json_body)
        }

        // Issue calibration token with Profiles scope
        let issue_body = serde_json::json!({
            "identity_id": "agent-profiles",
            "ttl_secs": 3600,
            "scopes": ["acoustics_read", "acoustics_write", "profiles"],
            "lease": {
                "baseline_revision": 1,
                "max_duration_secs": 300,
                "max_volume_pct": 50,
                "max_runs": 10
            }
        })
        .to_string();

        let req = format!(
            "POST /api/v1/acoustics/tokens HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer owner-secret-123\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, issue_body.len(), issue_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        let token = resp["token"].as_str().unwrap().to_string();

        // 1. Create room profile via agent token
        let default_dto = crate::acoustics::AcousticSettingsDto::from(
            crate::config::WakewordCalibrationConfig::default(),
        );
        let create_body = serde_json::json!({
            "id": "prof_bedroom",
            "name": "Спальня с коврами",
            "notes": "Мягкая мебель, мало эха",
            "settings": default_dto,
            "conditions": "quiet",
            "status": "draft"
        })
        .to_string();

        let req = format!(
            "POST /api/v1/acoustics/profiles HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nIdempotency-Key: idem-prof-1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, create_body.len(), create_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["profile"]["id"], "prof_bedroom");
        assert_eq!(resp["profile"]["name"], "Спальня с коврами");
        assert_eq!(resp["profile"]["revision"], 1);

        // 2. Duplicate create with same idempotency key returns same profile
        let (code_dup, resp_dup) = send_req(local_addr, &req).await;
        assert_eq!(code_dup, 200);
        assert_eq!(resp_dup["profile"]["id"], "prof_bedroom");
        assert_eq!(resp_dup["profile"]["revision"], 1);

        // 3. Agent cannot set owner_confirmed status (403 Forbidden)
        let owner_conf_body = serde_json::json!({
            "id": "prof_fraud",
            "name": "Фейк",
            "settings": default_dto,
            "status": "owner_confirmed"
        })
        .to_string();
        let req = format!(
            "POST /api/v1/acoustics/profiles HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, owner_conf_body.len(), owner_conf_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 403);
        assert!(resp["message"]
            .as_str()
            .unwrap()
            .contains("Only the owner can set owner_confirmed"));

        // 4. List profiles
        let req = format!(
            "GET /api/v1/acoustics/profiles HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            local_addr, token
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["profiles"].as_array().unwrap().len(), 1);

        // 5. Update profile with If-Match check
        let update_body = serde_json::json!({
            "name": "Спальня (проверено)",
            "status": "automated_tested"
        })
        .to_string();

        // Mismatched If-Match -> 412 Precondition Failed
        let req = format!(
            "PUT /api/v1/acoustics/profiles/prof_bedroom HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nIf-Match: \"999\"\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, update_body.len(), update_body
        );
        let (code, _) = send_req(local_addr, &req).await;
        assert_eq!(code, 412);

        // Correct If-Match -> 200 OK
        let req = format!(
            "PUT /api/v1/acoustics/profiles/prof_bedroom HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nIf-Match: \"1\"\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, update_body.len(), update_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["profile"]["revision"], 2);
        assert_eq!(resp["profile"]["name"], "Спальня (проверено)");
        assert_eq!(resp["profile"]["status"], "automated_tested");

        // 6. Preview candidate in RAM with TTL
        let preview_body = serde_json::json!({
            "settings": default_dto,
            "ttl_secs": 10
        })
        .to_string();
        let req = format!(
            "POST /api/v1/acoustics/preview HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, preview_body.len(), preview_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200, "Preview failed: {:?}", resp);
        assert_eq!(resp["preview"], true);

        // 7. Activate profile
        let req = format!(
            "POST /api/v1/acoustics/profiles/prof_bedroom/activate HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            local_addr, token
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["activated_profile_id"], "prof_bedroom");
        assert_eq!(
            profiles_store.get_active().unwrap().unwrap().id,
            "prof_bedroom"
        );

        // 8. Export profiles
        let req = format!(
            "POST /api/v1/acoustics/profiles/export HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            local_addr, token
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200);
        assert_eq!(resp["export"]["profiles"].as_array().unwrap().len(), 1);

        // 9. Anonymous caller gets 401 Unauthorized
        let req = format!(
            "POST /api/v1/acoustics/profiles HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, create_body.len(), create_body
        );
        let (code, _) = send_req(local_addr, &req).await;
        assert_eq!(code, 401);

        // Cleanup
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_http_samples_lifecycle_and_validation() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("http_samples_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let samples_store =
            Arc::new(crate::acoustics::samples::SamplesStore::new(temp_dir.clone()).unwrap());

        let auth_manager = Arc::new(AuthManager::new(Some("test_owner_secret".into())));
        let (_token_secret, cal_token) = auth_manager.issue_calibration_token(
            "test_calib_agent".into(),
            std::time::Duration::from_secs(3600),
            std::collections::HashSet::from([Scope::Samples, Scope::AcousticsRead]),
            CalibrationLease::default(),
        );
        let token = &cal_token.token;

        let ducker = Arc::new(crate::spotify::ducking::MusicDucker::new());
        let cfg_file = temp_dir.join("config.toml");
        std::fs::write(&cfg_file, "").unwrap();

        let server = Arc::new(
            ControlCenterServer::new(
                Some(cfg_file.clone()),
                "client_id".to_string(),
                "http://127.0.0.1/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                ducker,
            )
            .with_auth_manager(auth_manager.clone())
            .with_samples_store(samples_store.clone()),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let s = String::from_utf8_lossy(&buf);
            let code = s
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .parse::<u16>()
                .unwrap_or(0);
            let body_part = s.split("\r\n\r\n").nth(1).unwrap_or("{}");
            let json = serde_json::from_str(body_part).unwrap_or(serde_json::json!({}));
            (code, json)
        }

        async fn send_req_raw(addr: std::net::SocketAddr, raw_bytes: &[u8]) -> (u16, Vec<u8>) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw_bytes).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let mut split_pos = None;
            for i in 0..buf.len().saturating_sub(3) {
                if &buf[i..i + 4] == b"\r\n\r\n" {
                    split_pos = Some(i + 4);
                    break;
                }
            }
            let header_str = String::from_utf8_lossy(&buf[..split_pos.unwrap_or(0)]);
            let code = header_str
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .parse::<u16>()
                .unwrap_or(0);
            let body = split_pos.map(|p| buf[p..].to_vec()).unwrap_or_default();
            (code, body)
        }

        // 1. Prepare valid WAV (16kHz, mono, PCM16, 0.5s)
        let pcm = vec![1500i16; 8000];
        let wav_bytes = crate::acoustics::samples::encode_wav_pcm16_mono(&pcm).unwrap();
        use base64::Engine;
        let wav_b64 = base64::prelude::BASE64_STANDARD.encode(&wav_bytes);

        let create_body = serde_json::json!({
            "name": "Тестовое слово (Алиса)",
            "label": "positive",
            "split": "train",
            "interval": { "start_ms": 50, "end_ms": 450 },
            "notes": "Чистая запись",
            "wav_base64": wav_b64
        })
        .to_string();

        // 2. Upload valid sample via JSON
        let req = format!(
            "POST /api/v1/acoustics/samples HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, create_body.len(), create_body
        );
        let (code, resp) = send_req(local_addr, &req).await;
        assert_eq!(code, 200, "Upload failed: {:?}", resp);
        assert_eq!(resp["status"], "ok");
        let sample_id = resp["sample"]["id"].as_str().unwrap().to_string();
        assert!(sample_id.starts_with("smp_"));
        assert_eq!(resp["sample"]["metrics"]["channels"], 1);
        assert_eq!(resp["sample"]["metrics"]["sample_rate"], 16000);
        assert_eq!(resp["sample"]["metrics"]["duration_ms"], 500);
        assert_eq!(resp["sample"]["metrics"]["is_clipped"], false);

        // 3. Download WAV bytes and verify exact byte match
        let req_wav = format!(
            "GET /api/v1/acoustics/samples/{}/wav HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            sample_id, local_addr, token
        );
        let (code, downloaded_wav) = send_req_raw(local_addr, req_wav.as_bytes()).await;
        assert_eq!(code, 200);
        assert_eq!(downloaded_wav, wav_bytes);

        // 3b. Compute STFT spectrogram from stored sample
        let req_spec = format!(
            "GET /api/v1/acoustics/samples/{}/spectrogram HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            sample_id, local_addr, token
        );
        let (code, spec_resp) = send_req(local_addr, &req_spec).await;
        assert_eq!(code, 200, "Spectrogram failed: {:?}", spec_resp);
        assert_eq!(spec_resp["status"], "ok");
        assert_eq!(
            spec_resp["spectrogram"]["freq_bands_hz"]
                .as_array()
                .unwrap()
                .len(),
            32
        );
        assert!(!spec_resp["spectrogram"]["frames_db"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(spec_resp["spectrogram"]["band_energies_dbfs"]["full"].is_number());

        // 4. Get metadata
        let req_meta = format!(
            "GET /api/v1/acoustics/samples/{} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            sample_id, local_addr, token
        );
        let (code, meta_resp) = send_req(local_addr, &req_meta).await;
        assert_eq!(code, 200);
        assert_eq!(meta_resp["sample"]["name"], "Тестовое слово (Алиса)");
        assert_eq!(meta_resp["sample"]["split"], "train");

        // 5. Update sample
        let update_body = serde_json::json!({
            "name": "Алиса (обновлено)",
            "split": "held_out",
            "notes": "Перенесено в контроль"
        })
        .to_string();
        let req_upd = format!(
            "PUT /api/v1/acoustics/samples/{} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sample_id, local_addr, token, update_body.len(), update_body
        );
        let (code, upd_resp) = send_req(local_addr, &req_upd).await;
        assert_eq!(code, 200);
        assert_eq!(upd_resp["sample"]["name"], "Алиса (обновлено)");
        assert_eq!(upd_resp["sample"]["split"], "held_out");

        // 6. Upload invalid sample (stereo) -> 400 Bad Request
        let mut stereo_cursor = std::io::Cursor::new(Vec::new());
        {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 16000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::new(&mut stereo_cursor, spec).unwrap();
            writer.write_sample(100i16).unwrap();
            writer.write_sample(100i16).unwrap();
            writer.finalize().unwrap();
        }
        let stereo_b64 = base64::prelude::BASE64_STANDARD.encode(stereo_cursor.into_inner());
        let bad_body = serde_json::json!({
            "name": "Плохой файл",
            "label": "positive",
            "split": "train",
            "wav_base64": stereo_b64
        })
        .to_string();
        let req_bad = format!(
            "POST /api/v1/acoustics/samples HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, bad_body.len(), bad_body
        );
        let (code, bad_resp) = send_req(local_addr, &req_bad).await;
        assert_eq!(code, 400);
        assert!(bad_resp["message"].as_str().unwrap().contains("1 channel"));

        // 7. Upload sample with i16::MIN -> detected as clipping without panic
        let mut clipped_pcm = vec![500i16; 8000];
        clipped_pcm[400] = i16::MIN; // -32768
        let clipped_wav = crate::acoustics::samples::encode_wav_pcm16_mono(&clipped_pcm).unwrap();
        let clipped_b64 = base64::prelude::BASE64_STANDARD.encode(&clipped_wav);
        let clip_body = serde_json::json!({
            "name": "Клиппинг клип",
            "label": "positive",
            "split": "calibration",
            "wav_base64": clipped_b64
        })
        .to_string();
        let req_clip = format!(
            "POST /api/v1/acoustics/samples HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, clip_body.len(), clip_body
        );
        let (code, clip_resp) = send_req(local_addr, &req_clip).await;
        assert_eq!(code, 200);
        assert_eq!(clip_resp["sample"]["metrics"]["is_clipped"], true);
        assert_eq!(clip_resp["sample"]["metrics"]["peak_amplitude"], 32768);

        // 8. Delete sample
        let req_del = format!(
            "DELETE /api/v1/acoustics/samples/{} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            sample_id, local_addr, token
        );
        let (code, del_resp) = send_req(local_addr, &req_del).await;
        assert_eq!(code, 200);
        assert_eq!(del_resp["deleted"], sample_id);

        let (code, _) = send_req(local_addr, &req_meta).await;
        assert_eq!(code, 404);

        // 9. Anonymous caller gets 401 Unauthorized
        let anon_req = format!(
            "POST /api/v1/acoustics/samples HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, create_body.len(), create_body
        );
        let (code, _) = send_req(local_addr, &anon_req).await;
        assert_eq!(code, 401);

        // Cleanup
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_http_acoustic_jobs_lifecycle_and_conflict() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("mini_test_jobs_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let auth_manager = Arc::new(AuthManager::new(Some("test_owner_secret".to_string())));
        let (_token_secret, cal_token) = auth_manager.issue_calibration_token(
            "test_calib_agent".into(),
            std::time::Duration::from_secs(3600),
            std::collections::HashSet::from([Scope::Jobs, Scope::AcousticsRead]),
            crate::web::auth::CalibrationLease::default(),
        );
        let token = &cal_token.token;

        let samples_store =
            Arc::new(crate::acoustics::samples::SamplesStore::new(temp_dir.clone()).unwrap());
        let (lab, _rx) =
            crate::acoustics::AcousticLab::new(crate::config::WakewordCalibrationConfig::default());
        let mock_sink = Arc::new(crate::acoustics::jobs::MockPlaybackSink::new(false)); // AUX unsupported
        let job_runner = crate::acoustics::jobs::AcousticJobRunner::new(
            samples_store.clone(),
            lab.clone(),
            None,
            mock_sink.clone(),
        );

        let server = Arc::new(
            ControlCenterServer::new(
                None,
                "dummy_client".to_string(),
                "http://localhost/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                Arc::new(crate::spotify::ducking::MusicDucker::new()),
            )
            .with_auth_manager(auth_manager)
            .with_samples_store(samples_store)
            .with_acoustic_lab(lab)
            .with_job_runner(job_runner),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let s = String::from_utf8_lossy(&buf);
            let code = s
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let json_body = s
                .split_once("\r\n\r\n")
                .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b.trim()).ok())
                .unwrap_or(serde_json::Value::Null);
            (code, json_body)
        }

        // 1. GET /api/v1/acoustics/jobs initially empty
        let req_list = format!(
            "GET /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            local_addr, token
        );
        let (code, snap_resp) = send_req(local_addr, &req_list).await;
        assert_eq!(code, 200);
        assert_eq!(snap_resp["status"], "ok");
        assert!(snap_resp["active_job"].is_null());
        assert_eq!(snap_resp["aux_supported"], false);

        // 2. POST with AUX route when unsupported returns 422 Unprocessable Entity
        let aux_body = serde_json::json!({
            "name": "aux_job",
            "stimulus_origin": "external_user_position",
            "output_route": "aux_out",
            "stimulus": {
                "type": "silence",
                "duration_ms": 500
            },
            "pre_roll_ms": 100,
            "post_roll_ms": 100,
            "volume_pct": 40
        })
        .to_string();
        let req_aux = format!(
            "POST /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, aux_body.len(), aux_body
        );
        let (code, aux_resp) = send_req(local_addr, &req_aux).await;
        assert_eq!(code, 422);
        assert_eq!(aux_resp["code"], "unsupported");

        // 3. POST valid chirp sweep job -> 201 Created
        let valid_body = serde_json::json!({
            "name": "chirp_job",
            "stimulus_origin": "own_output",
            "output_route": "built_in",
            "stimulus": {
                "type": "chirp_sweep",
                "start_hz": 200.0,
                "end_hz": 2000.0,
                "duration_ms": 500,
                "volume_dbfs": -12.0
            },
            "pre_roll_ms": 100,
            "post_roll_ms": 100,
            "volume_pct": 50
        })
        .to_string();
        let req_start = format!(
            "POST /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, valid_body.len(), valid_body
        );
        let (code, start_resp) = send_req(local_addr, &req_start).await;
        assert_eq!(code, 201);
        let job_id = start_resp["job"]["job_id"].as_str().unwrap().to_string();
        assert!(!job_id.is_empty());

        // 4. Concurrent POST while first job is active -> 409 Conflict
        let req_conflict = format!(
            "POST /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, valid_body.len(), valid_body
        );
        let (code, conf_resp) = send_req(local_addr, &req_conflict).await;
        assert_eq!(code, 409);
        assert_eq!(conf_resp["code"], "conflict");

        // 5. GET /api/v1/acoustics/jobs/{id} returns job details
        let req_detail = format!(
            "GET /api/v1/acoustics/jobs/{} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            job_id, local_addr, token
        );
        let (code, detail_resp) = send_req(local_addr, &req_detail).await;
        assert_eq!(code, 200);
        assert_eq!(detail_resp["job"]["job_id"], job_id);

        // 6. POST /api/v1/acoustics/jobs/{id}/cancel
        let cancel_body = serde_json::json!({"reason": "Test abort"}).to_string();
        let req_cancel = format!(
            "POST /api/v1/acoustics/jobs/{}/cancel HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            job_id, local_addr, token, cancel_body.len(), cancel_body
        );
        let (code, cancel_resp) = send_req(local_addr, &req_cancel).await;
        assert_eq!(code, 200);
        assert_eq!(cancel_resp["job"]["state"], "cancelled");

        // 7. Anonymous request -> 401 Unauthorized
        let anon_req = format!(
            "POST /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, valid_body.len(), valid_body
        );
        let (code, _) = send_req(local_addr, &anon_req).await;
        assert_eq!(code, 401);

        // Cleanup
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_http_acoustic_evaluations_and_reports() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("mini_test_eval_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let auth_manager = Arc::new(AuthManager::new(Some("test_owner_secret".to_string())));
        let (_token_secret, cal_token) = auth_manager.issue_calibration_token(
            "test_eval_agent".into(),
            std::time::Duration::from_secs(3600),
            std::collections::HashSet::from([
                Scope::Jobs,
                Scope::Samples,
                Scope::AcousticsRead,
                Scope::AcousticsWrite,
            ]),
            crate::web::auth::CalibrationLease::default(),
        );
        let token = &cal_token.token;

        let samples_store =
            Arc::new(crate::acoustics::samples::SamplesStore::new(temp_dir.clone()).unwrap());
        let (lab, _rx) =
            crate::acoustics::AcousticLab::new(crate::config::WakewordCalibrationConfig::default());
        let mock_sink = Arc::new(crate::acoustics::jobs::MockPlaybackSink::new(false));
        let job_runner = crate::acoustics::jobs::AcousticJobRunner::new(
            samples_store.clone(),
            lab.clone(),
            None,
            mock_sink.clone(),
        );

        let server = Arc::new(
            ControlCenterServer::new(
                None,
                "dummy_client".to_string(),
                "http://localhost/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                Arc::new(crate::spotify::ducking::MusicDucker::new()),
            )
            .with_auth_manager(auth_manager)
            .with_samples_store(samples_store)
            .with_acoustic_lab(lab)
            .with_job_runner(job_runner),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let s = String::from_utf8_lossy(&buf);
            let code = s
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let json_body = s
                .split_once("\r\n\r\n")
                .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b.trim()).ok())
                .unwrap_or(serde_json::Value::Null);
            (code, json_body)
        }

        // 1. Upload a calibration sample
        let pcm = vec![3000i16; 8000];
        let wav = crate::acoustics::samples::encode_wav_pcm16_mono(&pcm).unwrap();
        use base64::Engine;
        let b64 = base64::prelude::BASE64_STANDARD.encode(&wav);
        let sample_body = serde_json::json!({
            "name": "eval_pos_sample",
            "label": "positive",
            "split": "calibration",
            "wav_base64": b64
        })
        .to_string();
        let req_smp = format!(
            "POST /api/v1/acoustics/samples HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, sample_body.len(), sample_body
        );
        let (code, _smp_resp) = send_req(local_addr, &req_smp).await;
        assert_eq!(code, 200);

        // 2. POST /api/v1/acoustics/evaluate
        let eval_body = serde_json::json!({
            "split": "calibration"
        })
        .to_string();
        let req_eval = format!(
            "POST /api/v1/acoustics/evaluate HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, eval_body.len(), eval_body
        );
        let (code, eval_resp) = send_req(local_addr, &req_eval).await;
        assert_eq!(code, 200);
        assert_eq!(eval_resp["status"], "ok");
        assert_eq!(eval_resp["evaluation"]["total_samples"], 1);
        assert_eq!(eval_resp["evaluation"]["positive_samples"], 1);

        // 3. POST /api/v1/acoustics/search
        let search_body = serde_json::json!({
            "split": "calibration",
            "max_trials": 4
        })
        .to_string();
        let req_srch = format!(
            "POST /api/v1/acoustics/search HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, search_body.len(), search_body
        );
        let (code, srch_resp) = send_req(local_addr, &req_srch).await;
        assert_eq!(code, 200);
        assert_eq!(srch_resp["status"], "ok");
        assert_eq!(srch_resp["search"]["total_trials"], 4);
        assert_eq!(
            srch_resp["search"]["ranked_candidates"]
                .as_array()
                .unwrap()
                .len(),
            4
        );

        // 4. Start job and get its report
        let job_body = serde_json::json!({
            "name": "job_for_report",
            "stimulus_origin": "ambient",
            "output_route": "none",
            "stimulus": {
                "type": "silence",
                "duration_ms": 200
            },
            "pre_roll_ms": 100,
            "post_roll_ms": 100,
            "volume_pct": 0
        })
        .to_string();
        let req_job = format!(
            "POST /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, job_body.len(), job_body
        );
        let (code, job_resp) = send_req(local_addr, &req_job).await;
        assert_eq!(code, 201);
        let job_id = job_resp["job"]["job_id"].as_str().unwrap().to_string();

        // Wait for job to finish
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let req_rep = format!(
            "GET /api/v1/acoustics/jobs/{}/report HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            job_id, local_addr, token
        );
        let (code, rep_resp) = send_req(local_addr, &req_rep).await;
        assert_eq!(code, 200);
        assert_eq!(rep_resp["status"], "ok");
        assert_eq!(rep_resp["report"]["job_id"], job_id);

        // 5. Anonymous evaluate request -> 401 Unauthorized
        let anon_req = format!(
            "POST /api/v1/acoustics/evaluate HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, eval_body.len(), eval_body
        );
        let (code, _) = send_req(local_addr, &anon_req).await;
        assert_eq!(code, 401);

        // Cleanup
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_http_acoustic_sessions_and_openapi_lifecycle() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("mini_test_sess_web_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let auth_manager = Arc::new(AuthManager::new(Some("test_owner_secret".to_string())));
        let (_token_secret, cal_token) = auth_manager.issue_calibration_token(
            "test_calib_agent".into(),
            std::time::Duration::from_secs(3600),
            std::collections::HashSet::from([
                Scope::Jobs,
                Scope::AcousticsRead,
                Scope::AcousticsWrite,
                Scope::Profiles,
                Scope::Samples,
            ]),
            crate::web::auth::CalibrationLease::default(),
        );
        let token = &cal_token.token;

        let samples_store = Arc::new(
            crate::acoustics::samples::SamplesStore::new(temp_dir.join("samples")).unwrap(),
        );
        let profiles_store = Arc::new(
            crate::acoustics::profiles::ProfilesStore::new(temp_dir.join("profiles.json")).unwrap(),
        );
        let session_manager =
            Arc::new(crate::acoustics::sessions::AcousticSessionManager::new(&temp_dir).unwrap());

        let (lab, mut rx) =
            crate::acoustics::AcousticLab::new(crate::config::WakewordCalibrationConfig::default());
        lab.ready(
            crate::config::WakewordCalibrationConfig::default(),
            crate::acoustics::RuntimeMetrics::default(),
        );
        let lab_mock = lab.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                lab_mock.acknowledge(req, None);
            }
        });
        let sink = Arc::new(crate::acoustics::jobs::MockPlaybackSink::new(false));
        let job_runner = crate::acoustics::jobs::AcousticJobRunner::new(
            samples_store.clone(),
            lab.clone(),
            None,
            sink,
        );

        let ducker = Arc::new(crate::spotify::ducking::MusicDucker::new());
        let cfg_file = temp_dir.join("config.toml");
        let server = Arc::new(
            ControlCenterServer::new(
                Some(cfg_file.clone()),
                "client_id".to_string(),
                "http://127.0.0.1/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                ducker,
            )
            .with_auth_manager(auth_manager.clone())
            .with_samples_store(samples_store.clone())
            .with_profiles_store(profiles_store.clone())
            .with_session_manager(session_manager.clone())
            .with_acoustic_lab(lab.clone())
            .with_job_runner(job_runner.clone()),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let s = String::from_utf8_lossy(&buf);
            let code = s
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .parse::<u16>()
                .unwrap_or(0);
            let json_body = s
                .split_once("\r\n\r\n")
                .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b.trim()).ok())
                .unwrap_or(serde_json::Value::Null);
            (code, json_body)
        }

        // 1. GET /api/v1/acoustics/openapi.json returns OpenAPI 3.1
        let req_schema = format!(
            "GET /api/v1/acoustics/openapi.json HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            local_addr
        );
        let (code, schema_resp) = send_req(local_addr, &req_schema).await;
        assert_eq!(code, 200);
        assert_eq!(schema_resp["openapi"], "3.1.0");
        assert!(schema_resp["paths"]["/state"].is_object());
        assert!(schema_resp["paths"]["/sessions"].is_object());

        // 2. GET /api/v1/acoustics/state returns snapshot with capabilities
        let req_state = format!(
            "GET /api/v1/acoustics/state HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            local_addr
        );
        let (code, state_resp) = send_req(local_addr, &req_state).await;
        assert_eq!(code, 200);
        assert_eq!(state_resp["status"], "ok");
        assert!(state_resp["capabilities"].is_array());

        // 3. POST /api/v1/acoustics/sessions creates bounded session
        let create_sess_body = serde_json::json!({
            "name": "Тестовая калибровка комнаты",
            "max_duration_secs": 1200,
            "max_jobs": 10,
            "require_aux": false
        })
        .to_string();
        let req_sess = format!(
            "POST /api/v1/acoustics/sessions HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, create_sess_body.len(), create_sess_body
        );
        let (code, sess_resp) = send_req(local_addr, &req_sess).await;
        assert_eq!(code, 201);
        assert_eq!(sess_resp["status"], "ok");
        let sess_id = sess_resp["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(sess_resp["session"]["state"], "running");

        // 3b. Duplicate session creation yields 409 Conflict
        let (code, conf_resp) = send_req(local_addr, &req_sess).await;
        assert_eq!(code, 409);
        assert_eq!(conf_resp["status"], "error");

        // 4. POST /sessions/{id}/candidate applies candidate in RAM
        let mut cand_dto = crate::acoustics::AcousticSettingsDto::from(
            crate::config::WakewordCalibrationConfig::default(),
        );
        cand_dto.threshold = 0.58;
        let cand_body = serde_json::json!({
            "settings": cand_dto
        })
        .to_string();
        let req_cand = format!(
            "POST /api/v1/acoustics/sessions/{}/candidate HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sess_id, local_addr, token, cand_body.len(), cand_body
        );
        let (code, cand_resp) = send_req(local_addr, &req_cand).await;
        assert_eq!(code, 200);
        assert_eq!(cand_resp["status"], "ok");

        // 5. Test idempotency on POST /api/v1/acoustics/jobs
        let job_body = serde_json::json!({
            "name": "idempotent_test_job",
            "stimulus_origin": "ambient",
            "output_route": "none",
            "stimulus": { "type": "silence", "duration_ms": 100 },
            "volume_pct": 0
        })
        .to_string();
        let req_job1 = format!(
            "POST /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nIdempotency-Key: idem_job_abc_123\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, job_body.len(), job_body
        );
        let (code1, resp_job1) = send_req(local_addr, &req_job1).await;
        assert_eq!(code1, 201);
        let job1_id = resp_job1["job"]["job_id"].as_str().unwrap().to_string();

        // Repeat identical request with same Idempotency-Key: must return identical job_id
        let req_job2 = format!(
            "POST /api/v1/acoustics/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nIdempotency-Key: idem_job_abc_123\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, job_body.len(), job_body
        );
        let (code2, resp_job2) = send_req(local_addr, &req_job2).await;
        assert_eq!(code2, 201);
        let job2_id = resp_job2["job"]["job_id"].as_str().unwrap().to_string();
        assert_eq!(job1_id, job2_id);

        // 6. POST /sessions/{id}/confirm_owner confirms live test
        let req_confirm = format!(
            "POST /api/v1/acoustics/sessions/{}/confirm_owner HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
            sess_id, local_addr, token
        );
        let (code, conf_owner_resp) = send_req(local_addr, &req_confirm).await;
        assert_eq!(code, 200);
        assert_eq!(conf_owner_resp["owner_confirmed"], true);

        // 7. POST /sessions/{id}/finish completes session and saves named profile
        let finish_body = serde_json::json!({
            "owner_confirmed": true,
            "save_profile_name": "Калиброванная спальня",
            "activate_profile": false
        })
        .to_string();
        let req_finish = format!(
            "POST /api/v1/acoustics/sessions/{}/finish HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sess_id, local_addr, token, finish_body.len(), finish_body
        );
        let (code, fin_resp) = send_req(local_addr, &req_finish).await;
        assert_eq!(code, 200);
        assert_eq!(fin_resp["status"], "ok");
        assert!(fin_resp["saved_profile_id"].is_string());

        // Cleanup
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_http_aec_telemetry_and_runtime_toggle() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("mini_test_aec_web_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let auth_manager = Arc::new(AuthManager::new(Some("owner_secret_aec".to_string())));
        let (_, cal_token) = auth_manager.issue_calibration_token(
            "calib_aec_agent".into(),
            std::time::Duration::from_secs(3600),
            std::collections::HashSet::from([Scope::AcousticsRead, Scope::AcousticsWrite]),
            crate::web::auth::CalibrationLease::default(),
        );
        let token = &cal_token.token;

        let (lab, _rx) =
            crate::acoustics::AcousticLab::new(crate::config::WakewordCalibrationConfig::default());
        let aec_cfg = crate::audio::AecConfig::default_mini();
        let aec_telem = crate::audio::SharedAecTelemetry::new(aec_cfg);
        aec_telem.publish(
            &crate::audio::AecMetrics {
                erle_db: 28.4,
                mic_rms_dbfs: -24.0,
                ref_rms_dbfs: -26.0,
                err_rms_dbfs: -52.4,
                double_talk_active: false,
            },
            40,
        );
        lab.set_aec_telemetry(aec_telem);

        let cfg_file = temp_dir.join("config.toml");
        std::fs::write(&cfg_file, "[audio]\nstartup_volume = 1\n").unwrap();

        let server = Arc::new(
            ControlCenterServer::new(
                Some(cfg_file.clone()),
                "client_id".to_string(),
                "http://127.0.0.1/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                Arc::new(crate::spotify::ducking::MusicDucker::new()),
            )
            .with_auth_manager(auth_manager)
            .with_acoustic_lab(lab),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let s = String::from_utf8_lossy(&buf);
            let code = s
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .parse::<u16>()
                .unwrap_or(0);
            let json_body = s
                .split_once("\r\n\r\n")
                .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b.trim()).ok())
                .unwrap_or(serde_json::Value::Null);
            (code, json_body)
        }

        // 1. Anonymous GET /api/v1/acoustics/aec -> 401
        let req_anon = format!(
            "GET /api/v1/acoustics/aec HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            local_addr
        );
        let (code, _) = send_req(local_addr, &req_anon).await;
        assert_eq!(code, 401);

        // 2. Calibration token GET /api/v1/acoustics/aec -> 200 with ACTIVE ERLE
        let req_get = format!(
            "GET /api/v1/acoustics/aec HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            local_addr, token
        );
        let (code, aec_resp) = send_req(local_addr, &req_get).await;
        assert_eq!(code, 200);
        assert_eq!(aec_resp["status"], "ok");
        assert_eq!(aec_resp["enabled"], true);
        assert_eq!(aec_resp["dtd_status"], "ACTIVE");
        assert!((aec_resp["erle_db"].as_f64().unwrap() - 28.4).abs() < 0.1);
        assert!(aec_resp["band_erle_db"]["full"].is_number());
        assert_eq!(aec_resp["spectrum_before_db"].as_array().unwrap().len(), 32);
        assert_eq!(aec_resp["spectrum_after_db"].as_array().unwrap().len(), 32);

        // 3. Calibration token cannot persist startup config (403)
        let persist_body = r#"{"enabled":false,"persist":true}"#;
        let req_cal_persist = format!(
            "POST /api/v1/acoustics/aec HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, persist_body.len(), persist_body
        );
        let (code, _) = send_req(local_addr, &req_cal_persist).await;
        assert_eq!(code, 403);

        // 4. Calibration token RAM-only toggle works (200)
        let ram_body = r#"{"enabled":false}"#;
        let req_cal_ram = format!(
            "POST /api/v1/acoustics/aec HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, token, ram_body.len(), ram_body
        );
        let (code, ram_resp) = send_req(local_addr, &req_cal_ram).await;
        assert_eq!(code, 200);
        assert_eq!(ram_resp["enabled"], false);
        assert_eq!(ram_resp["dtd_status"], "DISABLED");

        // 5. Owner can enable + persist to config.toml
        let owner_body = r#"{"enabled":true,"persist":true}"#;
        let req_owner_persist = format!(
            "POST /api/v1/acoustics/aec HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer owner_secret_aec\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            local_addr, owner_body.len(), owner_body
        );
        let (code, owner_resp) = send_req(local_addr, &req_owner_persist).await;
        assert_eq!(code, 200);
        assert_eq!(owner_resp["enabled"], true);
        let cfg_text = std::fs::read_to_string(&cfg_file).unwrap();
        assert!(cfg_text.contains("[audio.aec]"));
        assert!(cfg_text.contains("enabled = true"));

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_http_cue_volume_same_origin_and_convert() {
        use base64::Engine as _;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("mini_test_cue_web_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let auth_manager = Arc::new(AuthManager::new(Some("owner_secret_cue".to_string())));
        let lease = crate::web::auth::CalibrationLease {
            max_volume_pct: 50,
            ..Default::default()
        };
        let (_, cal_token) = auth_manager.issue_calibration_token(
            "calib_cue_agent".into(),
            std::time::Duration::from_secs(3600),
            Scope::all(),
            lease,
        );
        let token = &cal_token.token;

        let samples_store = Arc::new(
            crate::acoustics::samples::SamplesStore::new(temp_dir.join("samples")).unwrap(),
        );
        let profiles_store = Arc::new(
            crate::acoustics::profiles::ProfilesStore::new(temp_dir.join("profiles.json")).unwrap(),
        );
        let (lab, _rx) =
            crate::acoustics::AcousticLab::new(crate::config::WakewordCalibrationConfig::default());
        let sink = Arc::new(crate::acoustics::jobs::MockPlaybackSink::new(false));
        let job_runner = crate::acoustics::jobs::AcousticJobRunner::new(
            samples_store.clone(),
            lab.clone(),
            None,
            sink,
        );

        let ducker = Arc::new(crate::spotify::ducking::MusicDucker::new());
        let server = Arc::new(
            ControlCenterServer::new(
                Some(temp_dir.join("config.toml")),
                "client_id".to_string(),
                "http://127.0.0.1/callback".to_string(),
                Arc::new(RwLock::new(None)),
                None,
                ducker,
            )
            .with_auth_manager(auth_manager.clone())
            .with_samples_store(samples_store)
            .with_profiles_store(profiles_store)
            .with_acoustic_lab(lab)
            .with_job_runner(job_runner),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.run(listener, shutdown_rx).await;
            })
        };

        async fn send_req(addr: std::net::SocketAddr, raw: &str) -> (u16, serde_json::Value) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let s = String::from_utf8_lossy(&buf);
            let code = s
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .parse::<u16>()
                .unwrap_or(0);
            let json_body = s
                .split_once("\r\n\r\n")
                .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b.trim()).ok())
                .unwrap_or(serde_json::Value::Null);
            (code, json_body)
        }

        // 1. Anonymous GET without browser origin headers -> 401
        let req_anon = format!(
            "GET /api/v1/acoustics/profiles HTTP/1.1\r\nHost: {local_addr}\r\nConnection: close\r\n\r\n"
        );
        let (code, _) = send_req(local_addr, &req_anon).await;
        assert_eq!(code, 401);

        // 2. Browser same-origin GET via Sec-Fetch-Site or matching Referer -> 200
        let req_sec_fetch = format!(
            "GET /api/v1/acoustics/profiles HTTP/1.1\r\nHost: {local_addr}\r\nSec-Fetch-Site: same-origin\r\nConnection: close\r\n\r\n"
        );
        let (code, prof_resp) = send_req(local_addr, &req_sec_fetch).await;
        assert_eq!(code, 200);
        assert_eq!(prof_resp["status"], "ok");

        let req_referer = format!(
            "GET /api/v1/acoustics/samples HTTP/1.1\r\nHost: {local_addr}\r\nReferer: http://{local_addr}/acoustics\r\nConnection: close\r\n\r\n"
        );
        let (code, samp_resp) = send_req(local_addr, &req_referer).await;
        assert_eq!(code, 200);
        assert_eq!(samp_resp["status"], "ok");

        // Cross-site Referer stays blocked -> 401
        let req_evil = format!(
            "GET /api/v1/acoustics/profiles HTTP/1.1\r\nHost: {local_addr}\r\nReferer: http://evil.example/acoustics\r\nConnection: close\r\n\r\n"
        );
        let (code, _) = send_req(local_addr, &req_evil).await;
        assert_eq!(code, 401);

        // 3. POST /cue anonymous -> 401; calibrated clamps volume_pct to lease cap 50
        let cue_body = r#"{"kind":"double_beep","volume_pct":80}"#;
        let req_cue_anon = format!(
            "POST /api/v1/acoustics/cue HTTP/1.1\r\nHost: {local_addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{cue_body}",
            cue_body.len()
        );
        let (code, _) = send_req(local_addr, &req_cue_anon).await;
        assert_eq!(code, 401);

        let req_cue_cal = format!(
            "POST /api/v1/acoustics/cue HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{cue_body}",
            cue_body.len()
        );
        let (code, cue_resp) = send_req(local_addr, &req_cue_cal).await;
        assert_eq!(code, 200);
        assert_eq!(cue_resp["cue"]["volume_pct"], 50);
        assert_eq!(cue_resp["cue"]["kind"], "double_beep");
        assert!(cue_resp["cue"]["peak_amplitude"].as_i64().unwrap() > 0);

        // 4. GET & POST /volume with calibration lease clamp vs owner
        let req_vol_get = format!(
            "GET /api/v1/acoustics/volume HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        let (code, vol_get) = send_req(local_addr, &req_vol_get).await;
        assert_eq!(code, 200);
        assert_eq!(vol_get["max_volume_pct"], 50);

        let vol_high = r#"{"level":8}"#;
        let req_vol_cal = format!(
            "POST /api/v1/acoustics/volume HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{vol_high}",
            vol_high.len()
        );
        let (code, vol_cal_resp) = send_req(local_addr, &req_vol_cal).await;
        assert_eq!(code, 200);
        assert_eq!(vol_cal_resp["clamped"], true);
        assert_eq!(vol_cal_resp["level"], 2);
        assert_eq!(vol_cal_resp["soloist_pct"], 50);

        let vol_owner = r#"{"level":1}"#;
        let req_vol_owner = format!(
            "POST /api/v1/acoustics/volume HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer owner_secret_cue\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{vol_owner}",
            vol_owner.len()
        );
        let (code, vol_owner_resp) = send_req(local_addr, &req_vol_owner).await;
        assert_eq!(code, 200);
        assert_eq!(vol_owner_resp["clamped"], false);
        assert_eq!(vol_owner_resp["level"], 1);
        assert_eq!(vol_owner_resp["soloist_pct"], 42);

        // 5. GET & POST /reference_playback (soloist_client is None -> GET available=false, POST 503)
        let req_ref_get = format!(
            "GET /api/v1/acoustics/reference_playback HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        let (code, ref_get) = send_req(local_addr, &req_ref_get).await;
        assert_eq!(code, 200);
        assert_eq!(ref_get["available"], false);

        let ref_play = r#"{"action":"play"}"#;
        let req_ref_post = format!(
            "POST /api/v1/acoustics/reference_playback HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ref_play}",
            ref_play.len()
        );
        let (code, _) = send_req(local_addr, &req_ref_post).await;
        assert_eq!(code, 503);

        // 6. Auto-convert stereo 48kHz PCM16 WAV on upload + single-sample evaluate
        let mut stereo_samples = Vec::with_capacity(4800 * 2);
        for i in 0..4800usize {
            let v = ((i as f32 * 0.05).sin() * 12000.0) as i16;
            stereo_samples.push(v);
            stereo_samples.push(v);
        }
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut wav_cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut wav_cursor, spec).unwrap();
            for s in stereo_samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }
        let wav_b64 = base64::engine::general_purpose::STANDARD.encode(wav_cursor.into_inner());
        let upload_body = serde_json::json!({
            "name": "studio_48k_stereo_probe",
            "label": "positive",
            "split": "calibration",
            "wav_base64": wav_b64,
            "auto_convert": true
        })
        .to_string();
        let req_upload = format!(
            "POST /api/v1/acoustics/samples HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{upload_body}",
            upload_body.len()
        );
        let (code, up_resp) = send_req(local_addr, &req_upload).await;
        assert_eq!(code, 200);
        let sample_id = up_resp["sample"]["id"].as_str().unwrap().to_string();
        assert_eq!(up_resp["sample"]["metrics"]["sample_rate"], 16000);
        assert_eq!(up_resp["sample"]["metrics"]["channels"], 1);

        let eval_body = serde_json::json!({ "sample_id": sample_id }).to_string();
        let req_eval = format!(
            "POST /api/v1/acoustics/evaluate HTTP/1.1\r\nHost: {local_addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{eval_body}",
            eval_body.len()
        );
        let (code, eval_resp) = send_req(local_addr, &req_eval).await;
        assert_eq!(code, 200);
        assert_eq!(eval_resp["evaluation"]["total_samples"], 1);
        assert_eq!(
            eval_resp["evaluation"]["sample_results"][0]["sample_id"],
            sample_id
        );

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
