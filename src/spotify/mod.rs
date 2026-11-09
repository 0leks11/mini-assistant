//! Standalone Spotify Connect controller and Web API client for GPT Mini.

pub mod crypto;
pub mod local_server;
pub mod soloist;
pub mod web_api;

pub use local_server::{load_persisted_token, SpotifyWebServer};
#[allow(unused_imports)]
pub use soloist::{CurrentTrackInfo, PlaybackState, SoloistClient, SoloistStatus};
pub use web_api::SpotifyApiClient;

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

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
                return Some(serde_json::json!({ "status": "ok", "message": "Музыка поставлена на паузу" }));
            }
            // Fallback to Web API
            let mut web_guard = web_client_lock.write().await;
            if let Some(client) = web_guard.as_mut() {
                if let Ok(()) = client.pause(None).await {
                    info!("Spotify Web API pause executed successfully");
                    return Some(serde_json::json!({ "status": "ok", "message": "Музыка поставлена на паузу" }));
                }
            }
            Some(serde_json::json!({ "status": "error", "message": "Не удалось поставить музыку на паузу" }))
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
                let mut web_guard = web_client_lock.write().await;
                if let Some(client) = web_guard.as_mut() {
                    match client.search(q, "track,playlist", 1).await {
                        Ok(results) => {
                            if let Some(track) = results.tracks.first() {
                                if let Ok(()) = soloist.play_uri(&track.uri).await {
                                    let artist = track.artists.first().map(|a| a.name.as_str()).unwrap_or("");
                                    let msg = if artist.is_empty() {
                                        format!("Включаю {}", track.name)
                                    } else {
                                        format!("Включаю {} — {}", track.name, artist)
                                    };
                                    info!("Played track via local Soloist: {msg}");
                                    return Some(serde_json::json!({ "status": "ok", "message": msg }));
                                }
                            } else if let Some(playlist) = results.playlists.first() {
                                if let Ok(()) = soloist.play_uri(&playlist.uri).await {
                                    let msg = format!("Включаю плейлист {}", playlist.name);
                                    info!("Played playlist via local Soloist: {msg}");
                                    return Some(serde_json::json!({ "status": "ok", "message": msg }));
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Spotify Web API search failed: {e}; falling back to local resume");
                        }
                    }
                }
            }

            // Resume local soloist playback
            if let Ok(()) = soloist.resume().await {
                info!("Soloist resume executed successfully");
                return Some(
                    serde_json::json!({ "status": "ok", "message": "Воспроизведение возобновлено" }),
                );
            }

            // If resume didn't work (nothing queued), try playing default top hits playlist
            if let Ok(()) = soloist.play_uri("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M").await {
                info!("Soloist started default top hits playlist");
                return Some(
                    serde_json::json!({ "status": "ok", "message": "Включаю популярные треки" }),
                );
            }

            // Fallback to Web API resume / search_and_play
            let mut web_guard = web_client_lock.write().await;
            if let Some(client) = web_guard.as_mut() {
                if let Ok(()) = client.resume(None).await {
                    info!("Spotify Web API resume succeeded");
                    return Some(
                        serde_json::json!({ "status": "ok", "message": "Воспроизведение возобновлено" }),
                    );
                }
                // If resume fails (e.g. no active context), search and play top music
                let fallback_q = clean_query.unwrap_or("популярные треки");
                if let Ok(msg) = client.search_and_play(fallback_q).await {
                    info!("Spotify Web API fallback play succeeded: {msg}");
                    return Some(serde_json::json!({ "status": "ok", "message": msg }));
                }
            }

            Some(serde_json::json!({ "status": "error", "message": "Не удалось запустить музыку. Убедитесь, что Spotify Connect подключен к колонке." }))
        }
        "spotify_next" => {
            if let Ok(()) = soloist.next().await {
                info!("Soloist next executed successfully");
                return Some(serde_json::json!({ "status": "ok", "message": "Включен следующий трек" }));
            }
            let mut web_guard = web_client_lock.write().await;
            if let Some(client) = web_guard.as_mut() {
                if let Ok(()) = client.next(None).await {
                    info!("Spotify Web API next executed successfully");
                    return Some(serde_json::json!({ "status": "ok", "message": "Включен следующий трек" }));
                }
            }
            Some(serde_json::json!({ "status": "error", "message": "Не удалось переключить трек" }))
        }
        "spotify_previous" => {
            if let Ok(()) = soloist.previous().await {
                info!("Soloist previous executed successfully");
                return Some(serde_json::json!({ "status": "ok", "message": "Включен предыдущий трек" }));
            }
            let mut web_guard = web_client_lock.write().await;
            if let Some(client) = web_guard.as_mut() {
                if let Ok(()) = client.previous(None).await {
                    info!("Spotify Web API previous executed successfully");
                    return Some(serde_json::json!({ "status": "ok", "message": "Включен предыдущий трек" }));
                }
            }
            Some(serde_json::json!({ "status": "error", "message": "Не удалось переключить трек назад" }))
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
                        let artist_names: Vec<String> = item.artists.into_iter().map(|a| a.name).collect();
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
