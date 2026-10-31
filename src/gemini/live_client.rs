use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

const GEMINI_LIVE_URL: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerResponse {
    #[serde(rename = "serverContent")]
    pub server_content: Option<ServerContent>,
    pub interrupted: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerContent {
    #[serde(rename = "modelTurn")]
    pub model_turn: Option<ModelTurn>,
    #[serde(rename = "turnComplete")]
    pub turn_complete: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelTurn {
    pub parts: Option<Vec<Part>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Part {
    pub text: Option<String>,
    #[serde(rename = "inlineData")]
    pub inline_data: Option<InlineData>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InlineData {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
}

pub struct GeminiLiveClient {
    tx_audio: mpsc::Sender<Vec<u8>>,
}

impl GeminiLiveClient {
    /// Connect to Gemini 2.0 Live API over WebSocket.
    /// Spawns a background task that streams user PCM audio up and pushes model audio to playback.
    pub async fn connect(
        api_key: &str,
        model: &str,
        voice_name: &str,
        rx_user_audio: mpsc::Receiver<Vec<u8>>,
        tx_model_audio: mpsc::Sender<Vec<u8>>,
        on_interrupted: Arc<impl Fn() + Send + Sync + 'static>,
    ) -> Result<Self> {
        let url = format!("{}?key={}", GEMINI_LIVE_URL, api_key);
        info!("Connecting to Gemini Live API ({url_masked})...", url_masked = format!("{}?key=***", GEMINI_LIVE_URL));

        let (ws_stream, _) = connect_async(&url)
            .await
            .context("Failed to connect to Gemini Live WebSocket")?;
        info!("Connected to Gemini Live API WebSocket successfully");

        let (mut write, mut read) = ws_stream.split();

        // 1. Send Setup Handshake Message
        let setup_msg = json!({
            "setup": {
                "model": model,
                "generationConfig": {
                    "responseModalities": ["AUDIO"],
                    "speechConfig": {
                        "voiceConfig": {
                            "prebuiltVoiceConfig": {
                                "voiceName": voice_name
                            }
                        }
                    }
                },
                "systemInstruction": {
                    "parts": [
                        {
                            "text": "Ты — голосовой ассистент Яндекс Станции Мини. Отвечай кратко, живо, естественным разговорным голосом по-русски."
                        }
                    ]
                }
            }
        });

        write
            .send(Message::Text(setup_msg.to_string()))
            .await
            .context("Failed to send setup message")?;
        info!("Sent setup configuration to Gemini Live (voice: {})", voice_name);

        let (tx_audio, mut rx_audio_forward) = mpsc::channel::<Vec<u8>>(100);

        // Spawn Sender task (Mini PCM -> Gemini realtimeInput)
        tokio::spawn(async move {
            let mut rx = rx_user_audio;
            while let Some(chunk) = rx.recv().await {
                let base64_chunk = BASE64.encode(&chunk);
                let audio_payload = json!({
                    "realtimeInput": {
                        "mediaChunks": [
                            {
                                "mimeType": "audio/pcm;rate=16000",
                                "data": base64_chunk
                            }
                        ]
                    }
                });

                if let Err(e) = write.send(Message::Text(audio_payload.to_string())).await {
                    error!("Error sending audio chunk to Gemini Live: {}", e);
                    break;
                }
            }
        });

        // Spawn Receiver task (Gemini modelTurn -> Playback)
        tokio::spawn(async move {
            while let Some(msg_res) = read.next().await {
                match msg_res {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<ServerResponse>(&text) {
                            Ok(resp) => {
                                if resp.interrupted == Some(true) {
                                    info!("Gemini Live: User barge-in detected (interrupted)");
                                    on_interrupted();
                                }

                                if let Some(content) = resp.server_content {
                                    if let Some(turn) = content.model_turn {
                                        if let Some(parts) = turn.parts {
                                            for part in parts {
                                                if let Some(inline) = part.inline_data {
                                                    if inline.mime_type.starts_with("audio/pcm") {
                                                        if let Ok(raw_pcm) = BASE64.decode(&inline.data) {
                                                            let _ = tx_model_audio.send(raw_pcm).await;
                                                        }
                                                    }
                                                }
                                                if let Some(text_content) = part.text {
                                                    debug!("Gemini transcript: {}", text_content);
                                                }
                                            }
                                        }
                                    }

                                    if content.turn_complete == Some(true) {
                                        debug!("Gemini model turn complete");
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("Non-critical response parse: {} (raw: {})", e, text);
                            }
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        warn!("Gemini Live WebSocket closed by server: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        error!("Gemini Live WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(Self { tx_audio })
    }
}

use std::sync::Arc;
