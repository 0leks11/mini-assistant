//! Standalone Spotify Connect controller and Web API client for GPT Mini.

pub mod crypto;
pub mod ducking;
pub mod local_server;
pub mod soloist;
pub mod web_api;

pub use ducking::MusicDucker;
#[allow(unused_imports)]
pub use local_server::{load_persisted_token, SpotifyWebServer};
#[allow(unused_imports)]
pub use soloist::{CurrentTrackInfo, PlaybackState, SoloistClient, SoloistStatus};
pub use web_api::SpotifyApiClient;

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub const DEFAULT_TOP_HITS_PLAYLIST_URI: &str = "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSpotifyAction {
    Play,
    Pause,
    Next,
    Previous,
    Activate,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WebSpotifyResult {
    pub status: &'static str,
    pub message: String,
    pub is_active: bool,
    pub is_playing: bool,
}

/// Guarantee that local GPT Mini Soloist becomes the active Spotify Connect device
/// before issuing playback commands so playback never resumes on a laptop or phone.
pub async fn ensure_local_device_active(
    soloist: &SoloistClient,
    web_client_lock: &Arc<RwLock<Option<SpotifyApiClient>>>,
    start_play_on_transfer: bool,
) -> bool {
    if soloist.get_status().await.is_ok_and(|s| s.is_active) {
        return true;
    }

    if soloist.activate().await.is_ok() {
        for _ in 0..2 {
            tokio::time::sleep(tokio::time::Duration::from_millis(120)).await;
            if soloist.get_status().await.is_ok_and(|s| s.is_active) {
                return true;
            }
        }
    }

    let transferred = {
        let mut web_guard = web_client_lock.write().await;
        if let Some(client) = web_guard.as_mut() {
            client
                .transfer_playback_to_mini(start_play_on_transfer)
                .await
                .is_ok()
        } else {
            false
        }
    };

    if transferred {
        for _ in 0..3 {
            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
            if soloist.get_status().await.is_ok_and(|s| s.is_active) {
                return true;
            }
        }
    }

    soloist.get_status().await.is_ok_and(|s| s.is_active)
}

async fn find_mini_web_device_id(
    web_client_lock: &Arc<RwLock<Option<SpotifyApiClient>>>,
) -> Option<String> {
    let mut web_guard = web_client_lock.write().await;
    let client = web_guard.as_mut()?;
    let devices = client.get_devices().await.ok()?;
    web_api::find_mini_device(&devices).and_then(|d| d.id.clone())
}

async fn play_or_resume_on_mini(
    soloist: &SoloistClient,
    web_client_lock: &Arc<RwLock<Option<SpotifyApiClient>>>,
) -> Option<&'static str> {
    ensure_local_device_active(soloist, web_client_lock, false).await;

    let soloist_status = soloist.get_status().await.ok();
    let is_ended = soloist_status
        .as_ref()
        .map(|s| s.is_at_eof() || s.state == PlaybackState::Stopped)
        .unwrap_or(false);

    if !is_ended && soloist.resume().await.is_ok() {
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
        let actually_playing = soloist
            .get_status()
            .await
            .map(|s| s.is_actually_playing())
            .unwrap_or(false);

        if actually_playing {
            info!("Soloist resume executed successfully and playback is active on GPT Mini");
            return Some("Воспроизведение возобновлено");
        }
        warn!(
            "Soloist resume did not activate local playback (EOF or inactive device); starting default playlist on GPT Mini"
        );
    }

    if soloist
        .play_uri(DEFAULT_TOP_HITS_PLAYLIST_URI)
        .await
        .is_ok()
    {
        info!("Soloist started default top hits playlist on GPT Mini");
        return Some("Включаю популярные треки");
    }

    None
}

/// Unified handler for Web Dashboard Spotify actions (`play`, `pause`, `next`, `previous`, `activate`).
pub async fn handle_web_playback_action(
    soloist_opt: Option<&SoloistClient>,
    web_client_lock: &Arc<RwLock<Option<SpotifyApiClient>>>,
    action: WebSpotifyAction,
) -> WebSpotifyResult {
    let mut ok = false;
    let mut message = String::new();

    match action {
        WebSpotifyAction::Activate => {
            if let Some(soloist) = soloist_opt {
                ok = ensure_local_device_active(soloist, web_client_lock, false).await;
            } else {
                let mut web_guard = web_client_lock.write().await;
                if let Some(client) = web_guard.as_mut() {
                    ok = client.transfer_playback_to_mini(false).await.is_ok();
                }
            }
            message = if ok {
                "GPT Mini выбрана активным устройством Spotify".to_string()
            } else {
                "Не удалось переключить Spotify Connect на GPT Mini".to_string()
            };
        }
        WebSpotifyAction::Pause => {
            if let Some(soloist) = soloist_opt {
                if soloist.pause().await.is_ok() {
                    ok = true;
                    message = "Музыка поставлена на паузу".to_string();
                }
            }
            if !ok {
                let mut web_guard = web_client_lock.write().await;
                if let Some(client) = web_guard.as_mut() {
                    if client.pause(None).await.is_ok() {
                        ok = true;
                        message = "Музыка поставлена на паузу".to_string();
                    }
                }
            }
            if !ok {
                message = "Не удалось поставить музыку на паузу".to_string();
            }
        }
        WebSpotifyAction::Play => {
            if let Some(soloist) = soloist_opt {
                if let Some(msg) = play_or_resume_on_mini(soloist, web_client_lock).await {
                    ok = true;
                    message = msg.to_string();
                }
            }
            if !ok {
                if let Some(mini_id) = find_mini_web_device_id(web_client_lock).await {
                    let mut web_guard = web_client_lock.write().await;
                    if let Some(client) = web_guard.as_mut() {
                        if client.play(None, None, Some(&mini_id)).await.is_ok() {
                            ok = true;
                            message = "Воспроизведение возобновлено".to_string();
                        }
                    }
                }
            }
            if !ok {
                message =
                    "Не удалось запустить музыку на GPT Mini. Проверьте статус Spotify Connect."
                        .to_string();
            }
        }
        WebSpotifyAction::Next => {
            if let Some(soloist) = soloist_opt {
                ensure_local_device_active(soloist, web_client_lock, false).await;
                if soloist.next().await.is_ok()
                    || soloist
                        .play_uri(DEFAULT_TOP_HITS_PLAYLIST_URI)
                        .await
                        .is_ok()
                {
                    ok = true;
                    message = "Включен следующий трек".to_string();
                }
            }
            if !ok {
                if let Some(mini_id) = find_mini_web_device_id(web_client_lock).await {
                    let mut web_guard = web_client_lock.write().await;
                    if let Some(client) = web_guard.as_mut() {
                        if client.next(Some(&mini_id)).await.is_ok() {
                            ok = true;
                            message = "Включен следующий трек".to_string();
                        }
                    }
                }
            }
            if !ok {
                message = "Не удалось переключить трек на GPT Mini".to_string();
            }
        }
        WebSpotifyAction::Previous => {
            if let Some(soloist) = soloist_opt {
                ensure_local_device_active(soloist, web_client_lock, false).await;
                if soloist.previous().await.is_ok() {
                    ok = true;
                    message = "Включен предыдущий трек".to_string();
                }
            }
            if !ok {
                if let Some(mini_id) = find_mini_web_device_id(web_client_lock).await {
                    let mut web_guard = web_client_lock.write().await;
                    if let Some(client) = web_guard.as_mut() {
                        if client.previous(Some(&mini_id)).await.is_ok() {
                            ok = true;
                            message = "Включен предыдущий трек".to_string();
                        }
                    }
                }
            }
            if !ok {
                message = "Не удалось переключить трек назад на GPT Mini".to_string();
            }
        }
    }

    let status_snap = match soloist_opt {
        Some(soloist) => soloist.get_status().await.ok(),
        None => None,
    };

    WebSpotifyResult {
        status: if ok { "ok" } else { "error" },
        message,
        is_active: status_snap.as_ref().is_some_and(|s| s.is_active),
        is_playing: status_snap
            .as_ref()
            .is_some_and(|s| s.is_actually_playing()),
    }
}

/// Gemini tool declarations for Spotify playback controls.
pub fn spotify_tool_declarations() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "spotify_play",
            "description": "Воспроизведение музыки в Spotify или поиск и запуск трека/плейлиста по названию.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": {
                        "type": "STRING",
                        "description": "Поисковый запрос: название трека, исполнитель или плейлист (необязательно, если нужно просто возобновить воспроизведение)"
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "spotify_pause",
            "description": "Приостановить воспроизведение музыки в Spotify (пауза).",
            "parameters": {
                "type": "OBJECT",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "spotify_next",
            "description": "Переключить на следующий трек в Spotify.",
            "parameters": {
                "type": "OBJECT",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "spotify_previous",
            "description": "Переключить на предыдущий трек в Spotify.",
            "parameters": {
                "type": "OBJECT",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "spotify_volume",
            "description": "Установить громкость воспроизведения в Spotify (в процентах от 0 до 100).",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "volume_percent": {
                        "type": "INTEGER",
                        "description": "Уровень громкости от 0 до 100"
                    }
                },
                "required": ["volume_percent"]
            }
        }),
        serde_json::json!({
            "name": "spotify_current_track",
            "description": "Узнать, какой трек сейчас воспроизводится в Spotify.",
            "parameters": {
                "type": "OBJECT",
                "properties": {}
            }
        }),
    ]
}

/// Handles a tool call request for Spotify tools using Soloist and optional Web API client.
pub async fn handle_spotify_tool_call(
    soloist: &SoloistClient,
    web_client_lock: &Arc<RwLock<Option<SpotifyApiClient>>>,
    req: &crate::gemini::ToolCallRequest,
) -> Option<serde_json::Value> {
    match req.function_name.as_str() {
        "spotify_pause" => {
            if let Ok(()) = soloist.pause().await {
                info!("Soloist pause executed successfully");
                return Some(
                    serde_json::json!({ "status": "ok", "message": "Музыка поставлена на паузу" }),
                );
            }
            // Fallback to Web API
            let mut web_guard = web_client_lock.write().await;
            if let Some(client) = web_guard.as_mut() {
                if let Ok(()) = client.pause(None).await {
                    info!("Spotify Web API pause executed successfully");
                    return Some(
                        serde_json::json!({ "status": "ok", "message": "Музыка поставлена на паузу" }),
                    );
                }
            }
            Some(
                serde_json::json!({ "status": "error", "message": "Не удалось поставить музыку на паузу" }),
            )
        }
        "spotify_play" => {
            let query = req.args.get("query").and_then(|v| v.as_str());
            let clean_query = query.map(|s| s.trim()).filter(|s| !s.is_empty());
            let is_generic = clean_query
                .map(|q| {
                    let l = q.to_lowercase();
                    l == "музыка"
                        || l == "музыку"
                        || l == "музыки"
                        || l == "песня"
                        || l == "песню"
                        || l == "трек"
                        || l == "что-нибудь"
                        || l == "плейлист"
                })
                .unwrap_or(false);

            if let Some(q) = clean_query.filter(|_| !is_generic) {
                // Specific query: search via Web API and play directly through local Soloist speaker
                let search_res = {
                    let mut web_guard = web_client_lock.write().await;
                    match web_guard.as_mut() {
                        Some(client) => Some(client.search(q, "track,playlist", 1).await),
                        None => None,
                    }
                };
                if let Some(res) = search_res {
                    match res {
                        Ok(results) => {
                            ensure_local_device_active(soloist, web_client_lock, false).await;
                            if let Some(track) = results.tracks.first() {
                                if let Ok(()) = soloist.play_uri(&track.uri).await {
                                    let artist = track
                                        .artists
                                        .first()
                                        .map(|a| a.name.as_str())
                                        .unwrap_or("");
                                    let msg = if artist.is_empty() {
                                        format!("Включаю {}", track.name)
                                    } else {
                                        format!("Включаю {} — {}", track.name, artist)
                                    };
                                    info!("Played track via local Soloist: {msg}");
                                    return Some(
                                        serde_json::json!({ "status": "ok", "message": msg }),
                                    );
                                }
                            } else if let Some(playlist) = results.playlists.first() {
                                if let Ok(()) = soloist.play_uri(&playlist.uri).await {
                                    let msg = format!("Включаю плейлист {}", playlist.name);
                                    info!("Played playlist via local Soloist: {msg}");
                                    return Some(
                                        serde_json::json!({ "status": "ok", "message": msg }),
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Spotify Web API search failed: {e}; falling back to local resume"
                            );
                        }
                    }
                }
            }

            if let Some(msg) = play_or_resume_on_mini(soloist, web_client_lock).await {
                return Some(serde_json::json!({ "status": "ok", "message": msg }));
            }

            // Fallback to Web API explicitly targeted at GPT Mini device
            if let Some(mini_id) = find_mini_web_device_id(web_client_lock).await {
                let mut web_guard = web_client_lock.write().await;
                if let Some(client) = web_guard.as_mut() {
                    if let Ok(()) = client.resume(Some(&mini_id)).await {
                        info!("Spotify Web API resume on GPT Mini succeeded");
                        return Some(
                            serde_json::json!({ "status": "ok", "message": "Воспроизведение возобновлено" }),
                        );
                    }
                    let fallback_q = clean_query.unwrap_or("популярные треки");
                    if let Ok(results) = client.search(fallback_q, "track,playlist", 1).await {
                        if let Some(track) = results.tracks.first() {
                            let uris = [track.uri.as_str()];
                            if client.play(None, Some(&uris), Some(&mini_id)).await.is_ok() {
                                let artist =
                                    track.artists.first().map(|a| a.name.as_str()).unwrap_or("");
                                let msg = if artist.is_empty() {
                                    format!("Включаю {}", track.name)
                                } else {
                                    format!("Включаю {} — {}", track.name, artist)
                                };
                                info!("Spotify Web API fallback play on GPT Mini succeeded: {msg}");
                                return Some(serde_json::json!({ "status": "ok", "message": msg }));
                            }
                        } else if let Some(playlist) = results.playlists.first() {
                            if client
                                .play(Some(&playlist.uri), None, Some(&mini_id))
                                .await
                                .is_ok()
                            {
                                let msg = format!("Включаю плейлист {}", playlist.name);
                                return Some(serde_json::json!({ "status": "ok", "message": msg }));
                            }
                        }
                    }
                }
            }

            Some(
                serde_json::json!({ "status": "error", "message": "Не удалось запустить музыку на колонке. Убедитесь, что Spotify Connect подключен к GPT Mini." }),
            )
        }
        "spotify_next" => {
            ensure_local_device_active(soloist, web_client_lock, false).await;
            if let Ok(()) = soloist.next().await {
                info!("Soloist next executed successfully on GPT Mini");
                return Some(
                    serde_json::json!({ "status": "ok", "message": "Включен следующий трек" }),
                );
            }
            // Fallback: if Soloist had no queue (e.g. played single track to EOF), start top hits
            if let Ok(()) = soloist.play_uri(DEFAULT_TOP_HITS_PLAYLIST_URI).await {
                info!("Soloist next started default playlist fallback on GPT Mini");
                return Some(
                    serde_json::json!({ "status": "ok", "message": "Включаю следующий трек" }),
                );
            }
            if let Some(mini_id) = find_mini_web_device_id(web_client_lock).await {
                let mut web_guard = web_client_lock.write().await;
                if let Some(client) = web_guard.as_mut() {
                    if let Ok(()) = client.next(Some(&mini_id)).await {
                        info!("Spotify Web API next on GPT Mini executed successfully");
                        return Some(
                            serde_json::json!({ "status": "ok", "message": "Включен следующий трек" }),
                        );
                    }
                }
            }
            Some(serde_json::json!({ "status": "error", "message": "Не удалось переключить трек" }))
        }
        "spotify_previous" => {
            ensure_local_device_active(soloist, web_client_lock, false).await;
            if let Ok(()) = soloist.previous().await {
                info!("Soloist previous executed successfully on GPT Mini");
                return Some(
                    serde_json::json!({ "status": "ok", "message": "Включен предыдущий трек" }),
                );
            }
            if let Some(mini_id) = find_mini_web_device_id(web_client_lock).await {
                let mut web_guard = web_client_lock.write().await;
                if let Some(client) = web_guard.as_mut() {
                    if let Ok(()) = client.previous(Some(&mini_id)).await {
                        info!("Spotify Web API previous on GPT Mini executed successfully");
                        return Some(
                            serde_json::json!({ "status": "ok", "message": "Включен предыдущий трек" }),
                        );
                    }
                }
            }
            Some(
                serde_json::json!({ "status": "error", "message": "Не удалось переключить трек назад" }),
            )
        }
        "spotify_volume" => {
            let pct = req
                .args
                .get("volume_percent")
                .and_then(|v| v.as_u64())
                .map(|v| v.min(100) as u8)
                .unwrap_or(40);

            // Also update master software volume
            let level = crate::audio::volume::set_volume_pct(pct);

            let _ = soloist.set_volume(pct).await;
            let mut web_guard = web_client_lock.write().await;
            if let Some(client) = web_guard.as_mut() {
                let _ = client.set_volume(pct, None).await;
            }

            info!(volume = pct, level, "Spotify volume updated");
            Some(serde_json::json!({
                "status": "ok",
                "message": format!("Громкость установлена на {pct}% (уровень {level})")
            }))
        }
        "spotify_current_track" => {
            if let Ok(status) = soloist.get_status().await {
                if let Some(t) = status.track {
                    let msg = format!("Сейчас играет: {} — {}", t.artist, t.name);
                    return Some(serde_json::json!({
                        "status": "ok",
                        "artist": t.artist,
                        "title": t.name,
                        "album": t.album,
                        "message": msg
                    }));
                }
            }
            // Fallback to Web API playback state
            let mut web_guard = web_client_lock.write().await;
            if let Some(client) = web_guard.as_mut() {
                if let Ok(Some(state)) = client.get_playback_state().await {
                    if let Some(item) = state.item {
                        let artist_names: Vec<String> =
                            item.artists.into_iter().map(|a| a.name).collect();
                        let artist_str = artist_names.join(", ");
                        let msg = format!("Сейчас играет: {} — {}", artist_str, item.name);
                        return Some(serde_json::json!({
                            "status": "ok",
                            "artist": artist_str,
                            "title": item.name,
                            "message": msg
                        }));
                    }
                }
            }
            Some(serde_json::json!({
                "status": "ok",
                "message": "Сейчас ничего не играет"
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn create_fake_ctl(
        dir: &std::path::Path,
        initial_active: bool,
        activate_works: bool,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let state_file = dir.join("state.txt");
        let log_file = dir.join("calls.log");
        let ctl_path = dir.join("soloist-ctl");

        std::fs::write(
            &state_file,
            format!("paused {}", if initial_active { "true" } else { "false" }),
        )
        .unwrap();

        let script = format!(
            r#"#!/bin/sh
STATE_FILE="{state}"
LOG_FILE="{log}"
ACTIVATE_OK="{act}"
read CUR_STATUS CUR_ACTIVE < "$STATE_FILE"
echo "$*" >> "$LOG_FILE"
case "$1" in
  now)
    echo "{{\"type\":\"playback_state\",\"status\":\"$CUR_STATUS\",\"volume\":42,\"is_active\":$CUR_ACTIVE,\"position\":{{\"position_ms\":1000,\"speed\":0}},\"item\":{{\"uri\":\"spotify:track:1\",\"playback\":{{\"duration_ms\":180000}}}}}}"
    ;;
  activate)
    if [ "$ACTIVATE_OK" = "1" ]; then
      echo "$CUR_STATUS true" > "$STATE_FILE"
    fi
    ;;
  play)
    if [ $# -ge 2 ]; then
      echo "playing true" > "$STATE_FILE"
    else
      echo "playing $CUR_ACTIVE" > "$STATE_FILE"
    fi
    ;;
  pause)
    echo "paused $CUR_ACTIVE" > "$STATE_FILE"
    ;;
  next|prev)
    ;;
esac
"#,
            state = state_file.display(),
            log = log_file.display(),
            act = if activate_works { "1" } else { "0" }
        );
        std::fs::write(&ctl_path, script).unwrap();
        let mut perms = std::fs::metadata(&ctl_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&ctl_path, perms).unwrap();
        (ctl_path, state_file, log_file)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_spotify_play_activates_mini_before_resuming() {
        let dir = std::env::temp_dir().join(format!("sp_route_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ctl_path, _state_file, log_file) = create_fake_ctl(&dir, false, true);

        let soloist = SoloistClient::new(&ctl_path, "127.0.0.1:9090");
        let web_lock = Arc::new(RwLock::new(None::<SpotifyApiClient>));
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        let req = crate::gemini::ToolCallRequest {
            turn_id: 1,
            call_id: "1".into(),
            function_name: "spotify_play".into(),
            args: serde_json::json!({}),
            reply_tx,
        };

        let out = handle_spotify_tool_call(&soloist, &web_lock, &req)
            .await
            .expect("spotify_play response");
        assert_eq!(out["status"], "ok");

        let calls: Vec<String> = std::fs::read_to_string(&log_file)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let act_idx = calls
            .iter()
            .position(|c| c == "activate")
            .expect("activate must run");
        let play_idx = calls
            .iter()
            .position(|c| c == "play")
            .expect("play must run");
        assert!(
            act_idx < play_idx,
            "activate must run before play: {calls:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_spotify_pause_does_not_claim_inactive_device() {
        let dir = std::env::temp_dir().join(format!("sp_pause_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ctl_path, _state_file, log_file) = create_fake_ctl(&dir, false, true);

        let soloist = SoloistClient::new(&ctl_path, "127.0.0.1:9090");
        let web_lock = Arc::new(RwLock::new(None::<SpotifyApiClient>));
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        let req = crate::gemini::ToolCallRequest {
            turn_id: 2,
            call_id: "2".into(),
            function_name: "spotify_pause".into(),
            args: serde_json::json!({}),
            reply_tx,
        };

        let out = handle_spotify_tool_call(&soloist, &web_lock, &req)
            .await
            .expect("spotify_pause response");
        assert_eq!(out["status"], "ok");

        let calls: Vec<String> = std::fs::read_to_string(&log_file)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(calls, vec!["pause"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_ensure_local_device_active_uses_web_api_transfer_fallback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let dir = std::env::temp_dir().join(format!("sp_transfer_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // soloist-ctl activate cannot flip is_active on its own in this scenario
        let (ctl_path, state_file, log_file) = create_fake_ctl(&dir, false, false);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state_clone = state_file.clone();

        let server = tokio::spawn(async move {
            // 1. Token refresh (initial client has no cached access_token)
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await.unwrap();
                let body = serde_json::to_vec(&serde_json::json!({
                    "access_token": "mock-token",
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
            // 2. GET /v1/me/player/devices -> MacBook is active, GPT Mini is inactive
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /v1/me/player/devices"));
                let body = serde_json::to_vec(&serde_json::json!({
                    "devices": [
                        { "id": "macbook-id", "name": "MacBook Pro", "is_active": true, "volume_percent": 70 },
                        { "id": "mini-id", "name": "GPT Mini", "is_active": false, "volume_percent": 42 }
                    ]
                }))
                .unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
            // 3. PUT /v1/me/player transfers to mini-id and flips state to active
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                assert!(req.starts_with("PUT /v1/me/player "));
                let parsed: serde_json::Value =
                    serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
                assert_eq!(parsed["device_ids"], serde_json::json!(["mini-id"]));
                std::fs::write(&state_clone, "paused true").unwrap();
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }
        });

        let api_client = SpotifyApiClient::with_custom_endpoints(
            "cid".into(),
            "rt".into(),
            format!("http://{addr}/token"),
            format!("http://{addr}"),
        );
        let web_lock = Arc::new(RwLock::new(Some(api_client)));
        let soloist = SoloistClient::new(&ctl_path, "127.0.0.1:9090");

        let res =
            handle_web_playback_action(Some(&soloist), &web_lock, WebSpotifyAction::Play).await;
        assert_eq!(res.status, "ok");
        assert!(res.is_active);
        assert!(res.is_playing);

        let calls = std::fs::read_to_string(&log_file).unwrap();
        assert!(calls.contains("activate"));
        assert!(calls.contains("play"));

        let _ = server.await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
