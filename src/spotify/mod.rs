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
        "spotify_pause" => match soloist.pause().await {
            Ok(()) => {
                info!("Soloist pause executed successfully");
                Some(serde_json::json!({ "status": "ok", "message": "Музыка поставлена на паузу" }))
            }
            Err(e) => {
                warn!("Soloist pause failed: {e}");
                Some(serde_json::json!({ "status": "error", "message": e.to_string() }))
            }
        },
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
                // If specific query, try Web API search if authenticated
                let mut web_guard = web_client_lock.write().await;
                if let Some(client) = web_guard.as_mut() {
                    match client.search_and_play(q).await {
                        Ok(msg) => {
                            info!("Spotify search_and_play succeeded: {msg}");
                            return Some(serde_json::json!({ "status": "ok", "message": msg }));
                        }
                        Err(e) => {
                            warn!("Spotify Web API search failed: {e}; resuming local playback");
                        }
                    }
                }
            }

            // Resume local soloist playback
            match soloist.resume().await {
                Ok(()) => {
                    info!("Soloist resume executed successfully");
                    Some(
                        serde_json::json!({ "status": "ok", "message": "Воспроизведение возобновлено" }),
                    )
                }
                Err(e) => {
                    warn!("Soloist resume failed: {e}");
                    Some(serde_json::json!({ "status": "error", "message": e.to_string() }))
                }
            }
        }
        "spotify_next" => match soloist.next().await {
            Ok(()) => {
                info!("Soloist next executed successfully");
                Some(serde_json::json!({ "status": "ok", "message": "Включен следующий трек" }))
            }
            Err(e) => {
                warn!("Soloist next failed: {e}");
                Some(serde_json::json!({ "status": "error", "message": e.to_string() }))
            }
        },
        "spotify_previous" => match soloist.previous().await {
            Ok(()) => {
                info!("Soloist previous executed successfully");
                Some(serde_json::json!({ "status": "ok", "message": "Включен предыдущий трек" }))
            }
            Err(e) => {
                warn!("Soloist previous failed: {e}");
                Some(serde_json::json!({ "status": "error", "message": e.to_string() }))
            }
        },
        "spotify_volume" => {
            let pct = req
                .args
                .get("volume_percent")
                .and_then(|v| v.as_u64())
                .map(|v| v.min(100) as u8)
                .unwrap_or(40);
            match soloist.set_volume(pct).await {
                Ok(()) => {
                    info!(volume = pct, "Soloist volume set successfully");
                    Some(serde_json::json!({
                        "status": "ok",
                        "message": format!("Громкость установлена на {pct}%")
                    }))
                }
                Err(e) => {
                    warn!("Soloist set_volume failed: {e}");
                    Some(serde_json::json!({ "status": "error", "message": e.to_string() }))
                }
            }
        }
        "spotify_current_track" => match soloist.get_status().await {
            Ok(status) => {
                if let Some(t) = status.track {
                    let msg = format!("Сейчас играет: {} — {}", t.artist, t.name);
                    Some(serde_json::json!({
                        "status": "ok",
                        "artist": t.artist,
                        "title": t.name,
                        "album": t.album,
                        "message": msg
                    }))
                } else {
                    Some(serde_json::json!({
                        "status": "ok",
                        "message": "Сейчас ничего не играет"
                    }))
                }
            }
            Err(e) => {
                warn!("Soloist get_status failed: {e}");
                Some(serde_json::json!({ "status": "error", "message": e.to_string() }))
            }
        },
        _ => None,
    }
}
