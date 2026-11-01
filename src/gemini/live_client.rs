use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

const GEMINI_LIVE_URL: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerResponse {
    #[serde(rename = "serverContent")]
    pub server_content: Option<ServerContent>,
    #[serde(rename = "toolCall")]
    pub tool_call: Option<ToolCall>,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "functionCalls")]
    pub function_calls: Vec<FunctionCall>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

pub struct ToolCallRequest {
    pub call_id: String,
    pub function_name: String,
    pub args: Value,
    pub reply_tx: oneshot::Sender<Value>,
}

pub struct GeminiLiveClient {
    #[allow(dead_code)]
    pub tx_audio: mpsc::Sender<Vec<u8>>,
}

impl GeminiLiveClient {
    /// Connect to Gemini 2.0 Live API over WebSocket with real-time audio and tool execution.
    pub async fn connect(
        api_key: &str,
        model: &str,
        voice_name: &str,
        system_instruction_text: &str,
        has_ha_tools: bool,
        rx_user_audio: mpsc::Receiver<Vec<u8>>,
        tx_model_audio: mpsc::Sender<Vec<u8>>,
        tool_call_tx: Option<mpsc::Sender<ToolCallRequest>>,
        on_interrupted: Arc<impl Fn() + Send + Sync + 'static>,
    ) -> Result<Self> {
        let url = format!("{}?key={}", GEMINI_LIVE_URL, api_key);
        info!("Connecting to Gemini Live API ({url_masked})...", url_masked = format!("{}?key=***", GEMINI_LIVE_URL));

        let (ws_stream, _) = connect_async(&url)
            .await
            .context("Failed to connect to Gemini Live WebSocket")?;
        info!("Connected to Gemini Live API WebSocket successfully");

        let (mut write, mut read) = ws_stream.split();

        // 1. Build Tools Declaration for Home Assistant if enabled
        let mut tools = Vec::new();
        if has_ha_tools {
            tools.push(json!({
                "functionDeclarations": [
                    {
                        "name": "control_home_device",
                        "description": "Управление устройствами умного дома в Home Assistant (свет, кондиционер Midea, выключатели, розетки, режимы и температура).",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "domain": {
                                    "type": "STRING",
                                    "description": "Домен устройства (например 'climate', 'light', 'switch', 'media_player')"
                                },
                                "service": {
                                    "type": "STRING",
                                    "description": "Действие (например 'turn_on', 'turn_off', 'set_temperature', 'set_hvac_mode')"
                                },
                                "entity_id": {
                                    "type": "STRING",
                                    "description": "ID сущности Home Assistant (например 'climate.midea', 'light.living_room')"
                                },
                                "parameters": {
                                    "type": "OBJECT",
                                    "description": "Дополнительные параметры (например {'temperature': 22} или {'brightness': 200})"
                                }
                            },
                            "required": ["domain", "service", "entity_id"]
                        }
                    },
                    {
                        "name": "get_device_history",
                        "description": "Запрос истории работы устройства (например, сколько раз включался чайник, работа кондиционера).",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "entity_id": {
                                    "type": "STRING",
                                    "description": "ID сущности в Home Assistant"
                                },
                                "hours_ago": {
                                    "type": "INTEGER",
                                    "description": "За сколько прошедших часов запросить историю (по умолчанию 24)"
                                }
                            },
                            "required": ["entity_id"]
                        }
                    }
                ]
            }));
        }

        // 2. Build Setup Handshake Payload
        let mut setup_obj = json!({
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
                        "text": system_instruction_text
                    }
                ]
            }
        });

        if !tools.is_empty() {
            setup_obj.as_object_mut().unwrap().insert("tools".to_string(), Value::Array(tools));
        }

        let setup_msg = json!({ "setup": setup_obj });

        write
            .send(Message::Text(setup_msg.to_string()))
            .await
            .context("Failed to send setup message to Gemini Live")?;
        info!("Sent setup configuration to Gemini Live (voice: {}, ha_tools: {})", voice_name, has_ha_tools);

        let (tx_audio, mut rx_audio_forward) = mpsc::channel::<Vec<u8>>(100);
        let (tx_outgoing_ws, mut rx_outgoing_ws) = mpsc::channel::<String>(50);

        // Task 1: Outgoing WebSocket messages sender
        tokio::spawn(async move {
            let mut rx_user = rx_user_audio;
            loop {
                tokio::select! {
                    Some(chunk) = rx_user.recv() => {
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
                            error!("Error sending audio chunk: {}", e);
                            break;
                        }
                    }
                    Some(custom_msg) = rx_outgoing_ws.recv() => {
                        if let Err(e) = write.send(Message::Text(custom_msg)).await {
                            error!("Error sending custom WS message: {}", e);
                            break;
                        }
                    }
                    else => break,
                }
            }
        });

        // Task 2: Incoming WebSocket messages receiver & dispatcher
        tokio::spawn(async move {
            while let Some(msg_res) = read.next().await {
                match msg_res {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<ServerResponse>(&text) {
                            Ok(resp) => {
                                // Handle Barge-in Interruption
                                if resp.interrupted == Some(true) {
                                    info!("Gemini Live: Barge-in interruption triggered");
                                    on_interrupted();
                                }

                                // Handle Model Turn Audio Chunks
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

                                // Handle Tool Calls (Home Assistant functions)
                                if let Some(tc) = resp.tool_call {
                                    if let Some(ref handler_tx) = tool_call_tx {
                                        for call in tc.function_calls {
                                            info!("Gemini requested tool call: {} (id: {}) with args: {:?}", call.name, call.id, call.args);
                                            let (reply_tx, reply_rx) = oneshot::channel();
                                            let req = ToolCallRequest {
                                                call_id: call.id.clone(),
                                                function_name: call.name.clone(),
                                                args: call.args,
                                                reply_tx,
                                            };

                                            if handler_tx.send(req).await.is_ok() {
                                                if let Ok(result_output) = reply_rx.await {
                                                    let tool_resp_msg = json!({
                                                        "toolResponse": {
                                                            "functionResponses": [
                                                                {
                                                                    "response": {
                                                                        "output": result_output
                                                                    },
                                                                    "id": call.id
                                                                }
                                                            ]
                                                        }
                                                    });
                                                    let _ = tx_outgoing_ws.send(tool_resp_msg.to_string()).await;
                                                }
                                            }
                                        }
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
