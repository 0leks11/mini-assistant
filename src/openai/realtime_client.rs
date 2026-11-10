use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::gemini::{LiveClientConfig, SessionStatus, ToolCallRequest, UserAudio};
use crate::interaction::TurnAuthorization;

pub const OPENAI_REALTIME_URL: &str = "wss://api.openai.com/v1/realtime";

/// Convert a Gemini-style tool declaration to OpenAI function declaration.
pub fn convert_gemini_tool_to_openai(mut decl: Value) -> Value {
    let name = decl.get("name").cloned().unwrap_or(Value::Null);
    let desc = decl.get("description").cloned().unwrap_or(Value::Null);
    let mut params = decl
        .get_mut("parameters")
        .map(|p| p.take())
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

    if let Some(t) = params.get_mut("type") {
        if t.as_str() == Some("OBJECT") {
            *t = json!("object");
        }
    }

    json!({
        "type": "function",
        "name": name,
        "description": desc,
        "parameters": params,
    })
}

#[derive(Debug)]
pub struct OpenAiRealtimeClient {
    status: Arc<AtomicU8>,
    shutdown_tx: broadcast::Sender<()>,
    supervisor_handle: Option<tokio::task::JoinHandle<()>>,
}

impl OpenAiRealtimeClient {
    pub async fn connect(
        cfg: LiveClientConfig,
        rx_user_audio: mpsc::Receiver<UserAudio>,
        tx_model_audio: mpsc::Sender<(u64, u64, Vec<u8>)>,
        tool_call_tx: Option<mpsc::Sender<ToolCallRequest>>,
        on_interrupted: Arc<impl Fn() + Send + Sync + 'static>,
    ) -> Result<Self> {
        if let Some(a) = &cfg.authorization {
            a.cancel();
        }
        let status = Arc::new(AtomicU8::new(SessionStatus::Connecting as u8));
        let (shutdown_tx, _) = broadcast::channel(1);
        let (initial_tx, initial_rx) = oneshot::channel::<Result<()>>();

        let status_for_supervisor = status.clone();
        let shutdown_tx_for_supervisor = shutdown_tx.clone();
        let cfg_clone = cfg.clone();

        let supervisor_handle = tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            let mut initial_tx = Some(initial_tx);
            let mut shutdown_rx = shutdown_tx_for_supervisor.subscribe();
            let cancellation_epoch = cfg_clone
                .cancellation_epoch
                .clone()
                .unwrap_or_else(|| Arc::new(AtomicU64::new(0)));
            let local_epoch = cfg_clone
                .cancellation_epoch
                .is_none()
                .then(|| cancellation_epoch.clone());
            let on_interrupted = Arc::new(move || {
                on_interrupted();
                if let Some(epoch) = &local_epoch {
                    epoch.fetch_add(1, Ordering::SeqCst);
                }
            });
            let mut rx_user_audio = rx_user_audio;

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    status_for_supervisor.store(SessionStatus::Stopped as u8, Ordering::SeqCst);
                    break;
                }

                status_for_supervisor.store(SessionStatus::Connecting as u8, Ordering::SeqCst);
                let session_res = run_single_session(
                    &cfg_clone,
                    &mut rx_user_audio,
                    &tx_model_audio,
                    &tool_call_tx,
                    on_interrupted.clone(),
                    cancellation_epoch.clone(),
                    status_for_supervisor.clone(),
                    shutdown_tx_for_supervisor.clone(),
                    &mut initial_tx,
                )
                .await;

                if shutdown_rx.try_recv().is_ok() {
                    status_for_supervisor.store(SessionStatus::Stopped as u8, Ordering::SeqCst);
                    break;
                }

                if let Some(tx) = initial_tx.take() {
                    let err = match session_res {
                        Err(e) => e,
                        Ok(()) => anyhow::anyhow!(
                            "OpenAI Realtime session closed before setup confirmation"
                        ),
                    };
                    status_for_supervisor.store(SessionStatus::Stopped as u8, Ordering::SeqCst);
                    let _ = tx.send(Err(err));
                    break;
                }

                if let Some(a) = &cfg_clone.authorization {
                    a.cancel();
                }
                while rx_user_audio.try_recv().is_ok() {}
                status_for_supervisor.store(SessionStatus::Degraded as u8, Ordering::SeqCst);
                on_interrupted();

                match session_res {
                    Ok(()) => {
                        info!("OpenAI Realtime session closed normally, reconnecting in {backoff_secs}s...");
                    }
                    Err(e) => {
                        warn!("OpenAI Realtime session dropped ({e:#}), reconnecting in {backoff_secs}s...");
                    }
                }

                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {
                        backoff_secs = (backoff_secs * 2).min(10);
                    }
                    _ = shutdown_rx.recv() => {
                        status_for_supervisor.store(SessionStatus::Stopped as u8, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });

        // Wait for initial handshake or timeout
        let initial_timeout = std::time::Duration::from_secs(15);
        match tokio::time::timeout(initial_timeout, initial_rx).await {
            Ok(Ok(Ok(()))) => {
                info!("OpenAI Realtime client connected and ready");
                Ok(Self {
                    status,
                    shutdown_tx,
                    supervisor_handle: Some(supervisor_handle),
                })
            }
            Ok(Ok(Err(e))) => {
                let _ = shutdown_tx.send(());
                supervisor_handle.abort();
                Err(e).context("OpenAI Realtime initial setup failed")
            }
            Ok(Err(_)) => {
                let _ = shutdown_tx.send(());
                supervisor_handle.abort();
                anyhow::bail!("OpenAI Realtime supervisor closed before confirming setup")
            }
            Err(_) => {
                let _ = shutdown_tx.send(());
                supervisor_handle.abort();
                anyhow::bail!("Timed out waiting for initial OpenAI Realtime handshake (15s)")
            }
        }
    }

    pub fn status_handle(&self) -> Arc<AtomicU8> {
        self.status.clone()
    }

    pub fn status(&self) -> SessionStatus {
        SessionStatus::from(self.status.load(Ordering::SeqCst))
    }

    pub async fn verify_handshake(cfg: &LiveClientConfig) -> Result<()> {
        let model = if cfg.model.trim().is_empty() {
            "gpt-4o-realtime-preview"
        } else {
            &cfg.model
        };
        let url_str = match &cfg.endpoint {
            Some(custom) => custom.clone(),
            None => format!("{}?model={}", OPENAI_REALTIME_URL, model),
        };
        let mut req = url_str
            .as_str()
            .into_client_request()
            .context("Failed to build OpenAI Realtime client request")?;
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", cfg.api_key)
                .parse()
                .context("Invalid authorization header")?,
        );
        req.headers_mut().insert(
            "OpenAI-Beta",
            "realtime=v1"
                .parse()
                .context("Invalid OpenAI-Beta header")?,
        );
        let (ws_stream, _) = connect_async(req)
            .await
            .context("Failed to connect to OpenAI Realtime WebSocket")?;
        let (mut write, mut read) = ws_stream.split();

        let voice = if cfg.voice_name.trim().is_empty() {
            "alloy"
        } else {
            cfg.voice_name.as_str()
        };

        let setup_msg = json!({
            "type": "session.update",
            "session": {
                "modalities": ["text", "audio"],
                "voice": voice,
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "turn_detection": null,
            }
        });

        write
            .send(Message::Text(setup_msg.to_string()))
            .await
            .context("Failed to send session.update to OpenAI Realtime")?;

        let setup_timeout = std::time::Duration::from_secs(10);
        tokio::time::timeout(setup_timeout, async {
            while let Some(msg_res) = read.next().await {
                let frame = msg_res.context("WebSocket error during setup handshake")?;
                match &frame {
                    Message::Text(text) => {
                        if let Ok(resp) = serde_json::from_str::<Value>(text) {
                            let msg_type = resp.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if msg_type == "session.updated" || msg_type == "session.created" {
                                return Ok(());
                            }
                            if msg_type == "error" {
                                let err_msg = resp
                                    .get("error")
                                    .and_then(|e| e.get("message"))
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown error");
                                anyhow::bail!("OpenAI Realtime setup rejected: {err_msg}");
                            }
                        }
                    }
                    Message::Close(cf) => {
                        anyhow::bail!(
                            "WebSocket closed by server during setup handshake: {:?}",
                            cf
                        );
                    }
                    _ => continue,
                }
            }
            anyhow::bail!("WebSocket closed before session.updated confirmation");
        })
        .await
        .context("Timed out waiting for OpenAI session.updated")??;

        Ok(())
    }

    pub async fn stop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(h) = self.supervisor_handle.take() {
            h.abort();
        }
        self.status
            .store(SessionStatus::Stopped as u8, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_single_session(
    cfg: &LiveClientConfig,
    rx_user_audio: &mut mpsc::Receiver<UserAudio>,
    tx_model_audio: &mpsc::Sender<(u64, u64, Vec<u8>)>,
    tool_call_tx: &Option<mpsc::Sender<ToolCallRequest>>,
    _on_interrupted: Arc<dyn Fn() + Send + Sync + 'static>,
    cancellation_epoch: Arc<AtomicU64>,
    status: Arc<AtomicU8>,
    shutdown_tx: broadcast::Sender<()>,
    initial_tx: &mut Option<oneshot::Sender<Result<()>>>,
) -> Result<()> {
    let model = if cfg.model.trim().is_empty() {
        "gpt-4o-realtime-preview"
    } else {
        &cfg.model
    };

    let url_str = match &cfg.endpoint {
        Some(custom) => custom.clone(),
        None => format!("{}?model={}", OPENAI_REALTIME_URL, model),
    };

    info!(
        "Connecting to OpenAI Realtime API (model: {}, voice: {})...",
        model, cfg.voice_name
    );

    let mut req = url_str
        .as_str()
        .into_client_request()
        .context("Failed to build OpenAI Realtime client request")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", cfg.api_key)
            .parse()
            .context("Invalid authorization header")?,
    );
    req.headers_mut().insert(
        "OpenAI-Beta",
        "realtime=v1"
            .parse()
            .context("Invalid OpenAI-Beta header")?,
    );

    let (ws_stream, _) = match connect_async(req).await {
        Ok(res) => res,
        Err(e) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now < 1_767_225_600 {
                warn!(
                    unix_timestamp = now,
                    "OpenAI Realtime WebSocket connect failed and system clock appears out of sync (< 2026)"
                );
            }
            return Err(e).context("Failed to connect to OpenAI Realtime WebSocket");
        }
    };

    info!("Connected to OpenAI Realtime WebSocket successfully");
    let (mut write, mut read) = ws_stream.split();

    // Build OpenAI tools array
    let mut tools = Vec::new();
    if cfg.has_ha_tools {
        tools.push(json!({
            "type": "function",
            "name": "control_home_device",
            "description": "Управление устройствами умного дома в Home Assistant (свет, кондиционер Midea, выключатели, розетки, режимы и температура).",
            "parameters": {
                "type": "object",
                "properties": {
                    "domain": { "type": "string", "description": "Домен (light, climate, switch)" },
                    "service": { "type": "string", "description": "Команда (turn_on, turn_off, set_temperature)" },
                    "entity_id": { "type": "string", "description": "ID устройства" },
                    "service_data": { "type": "object", "description": "Параметры команды" }
                },
                "required": ["domain", "service", "entity_id"]
            }
        }));
        tools.push(json!({
            "type": "function",
            "name": "get_device_history",
            "description": "Запрос истории работы устройства (например, кондиционера).",
            "parameters": {
                "type": "object",
                "properties": {
                    "entity_id": { "type": "string", "description": "ID устройства" },
                    "hours": { "type": "integer", "description": "Период в часах" }
                },
                "required": ["entity_id"]
            }
        }));
    }
    if cfg.has_weather_tools {
        tools.push(convert_gemini_tool_to_openai(
            crate::weather::WeatherService::tool_declaration(),
        ));
    }
    if cfg.has_alarm_tools {
        for tool in crate::alarms::alarm_tool_declarations() {
            tools.push(convert_gemini_tool_to_openai(tool));
        }
    }
    if cfg.has_spotify_tools {
        for tool in crate::spotify::spotify_tool_declarations() {
            tools.push(convert_gemini_tool_to_openai(tool));
        }
    }
    if cfg.has_volume_tools {
        tools.push(json!({
            "type": "function",
            "name": "set_volume",
            "description": "Установить громкость колонки от 0 до 10. 0 = тишина, 10 = максимум.",
            "parameters": {
                "type": "object",
                "properties": {
                    "level": { "type": "integer", "description": "Уровень громкости от 0 до 10" }
                },
                "required": ["level"]
            }
        }));
        tools.push(json!({
            "type": "function",
            "name": "volume_up",
            "description": "Сделать громче на 1 ступень.",
            "parameters": { "type": "object", "properties": {} }
        }));
        tools.push(json!({
            "type": "function",
            "name": "volume_down",
            "description": "Сделать тише на 1 ступень.",
            "parameters": { "type": "object", "properties": {} }
        }));
    }
    if cfg.has_bluetooth_tools {
        tools.push(json!({
            "type": "function",
            "name": "enable_bluetooth",
            "description": "Включить Bluetooth для поиска и подключения телефона или ноутбука к колонке.",
            "parameters": { "type": "object", "properties": {} }
        }));
    }
    if cfg.has_stop_dialogue {
        tools.push(json!({
            "type": "function",
            "name": "stop_dialogue",
            "description": "Завершить диалог или остановить разговор/воспроизведение. Вызывай немедленно при фразах «хватит», «стоп», «молчи», «замолчи».",
            "parameters": { "type": "object", "properties": {} }
        }));
    }

    let voice = if cfg.voice_name.trim().is_empty() {
        "alloy"
    } else {
        cfg.voice_name.as_str()
    };

    let setup_msg = json!({
        "type": "session.update",
        "session": {
            "modalities": ["text", "audio"],
            "instructions": cfg.system_instruction,
            "voice": voice,
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "turn_detection": null,
            "tools": tools,
            "tool_choice": "auto",
            "temperature": 0.7
        }
    });

    write
        .send(Message::Text(setup_msg.to_string()))
        .await
        .context("Failed to send session.update to OpenAI Realtime")?;

    // Wait for session.updated confirmation
    let setup_timeout = std::time::Duration::from_secs(10);
    tokio::time::timeout(setup_timeout, async {
        while let Some(msg_res) = read.next().await {
            let frame = msg_res.context("WebSocket error during setup handshake")?;
            match &frame {
                Message::Text(text) => {
                    if let Ok(resp) = serde_json::from_str::<Value>(text) {
                        let msg_type = resp.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if msg_type == "session.updated" || msg_type == "session.created" {
                            return Ok(());
                        }
                        if msg_type == "error" {
                            let err_msg = resp
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error");
                            anyhow::bail!("OpenAI Realtime setup rejected: {err_msg}");
                        }
                    }
                }
                Message::Close(cf) => {
                    anyhow::bail!(
                        "WebSocket closed by server during setup handshake: {:?}",
                        cf
                    );
                }
                _ => continue,
            }
        }
        anyhow::bail!("WebSocket closed before session.updated confirmation");
    })
    .await
    .context("Timed out waiting for OpenAI session.updated")??;

    info!("OpenAI Realtime setup handshake completed successfully");
    if let Some(auth) = &cfg.authorization {
        auth.cancel();
    }
    while rx_user_audio.try_recv().is_ok() {}

    status.store(SessionStatus::Ready as u8, Ordering::SeqCst);
    if let Some(tx) = initial_tx.take() {
        let _ = tx.send(Ok(()));
    }

    let (tx_outgoing_ws, rx_outgoing_ws) = mpsc::channel::<(u64, String)>(50);

    let sender = run_sender_task(
        write,
        rx_user_audio,
        rx_outgoing_ws,
        cfg.authorization.clone(),
        shutdown_tx.subscribe(),
    );

    let receiver = run_receiver_task(
        read,
        tx_model_audio.clone(),
        tool_call_tx.clone(),
        tx_outgoing_ws,
        cancellation_epoch,
        cfg.authorization.clone(),
        shutdown_tx.subscribe(),
    );

    tokio::select! {
        res = sender => res,
        res = receiver => res,
    }
}

async fn run_sender_task<S>(
    mut write: S,
    rx_user: &mut mpsc::Receiver<UserAudio>,
    mut rx_outgoing_ws: mpsc::Receiver<(u64, String)>,
    authorization: Option<Arc<TurnAuthorization>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(30));
    ping.tick().await;
    let mut submitted_turn = None;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                write.send(Message::Ping(Vec::new())).await?;
            }
            maybe = rx_user.recv() => {
                let Some(input) = maybe else { return Ok(()) };
                let Some(auth) = &authorization else { continue };
                if !auth.is_current(input.turn_id) { continue; }

                if input.pcm.is_empty() {
                    // Turn finished - commit buffer and request response
                    if submitted_turn == Some(input.turn_id) {
                        let _ = auth.complete_input(input.turn_id);
                        submitted_turn = None;
                        write.send(Message::Text(json!({
                            "type": "input_audio_buffer.commit"
                        }).to_string())).await?;
                        write.send(Message::Text(json!({
                            "type": "response.create"
                        }).to_string())).await?;
                    }
                } else {
                    // Audio chunk
                    submitted_turn = Some(input.turn_id);
                    let b64 = BASE64.encode(&input.pcm);
                    write.send(Message::Text(json!({
                        "type": "input_audio_buffer.append",
                        "audio": b64
                    }).to_string())).await?;
                }
            }
            Some((turn_id, message)) = rx_outgoing_ws.recv() => {
                if authorization.as_ref().is_some_and(|a| a.is_current(turn_id)) {
                    write.send(Message::Text(message)).await?;
                }
            }
            _ = shutdown_rx.recv() => return Ok(()),
        }
    }
}

async fn run_receiver_task<S>(
    mut read: S,
    tx_model_audio: mpsc::Sender<(u64, u64, Vec<u8>)>,
    tool_call_tx: Option<mpsc::Sender<ToolCallRequest>>,
    tx_outgoing_ws: mpsc::Sender<(u64, String)>,
    cancellation_epoch: Arc<AtomicU64>,
    authorization: Option<Arc<TurnAuthorization>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    S: futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    let mut current_turn_id = 0u64;

    loop {
        tokio::select! {
            frame = read.next() => {
                let Some(msg_res) = frame else { return Ok(()) };
                let msg = msg_res?;
                let text = match &msg {
                    Message::Text(t) => t.as_str(),
                    Message::Binary(b) => match std::str::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    Message::Close(_) => return Ok(()),
                    _ => continue,
                };

                let Ok(resp) = serde_json::from_str::<Value>(text) else { continue };
                let msg_type = resp.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match msg_type {
                    "response.created" => {
                        if let Some(auth) = &authorization {
                            if let Some(active_id) = auth.current_turn() {
                                current_turn_id = active_id;
                            }
                        }
                    }
                    "response.audio.delta" => {
                        if let Some(delta) = resp.get("delta").and_then(|d| d.as_str()) {
                            if let Ok(pcm_bytes) = BASE64.decode(delta) {
                                let epoch = cancellation_epoch.load(Ordering::SeqCst);
                                let _ = tx_model_audio.send((epoch, current_turn_id, pcm_bytes)).await;
                            }
                        }
                    }
                    "response.audio.done" => {
                        let epoch = cancellation_epoch.load(Ordering::SeqCst);
                        let _ = tx_model_audio.send((epoch, current_turn_id, Vec::new())).await;
                    }
                    "response.function_call_arguments.done" => {
                        let call_id = resp.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = resp.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let arguments_str = resp.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                        let args: Value = serde_json::from_str(arguments_str).unwrap_or(json!({}));

                        if let Some(ref handler) = tool_call_tx {
                            let (reply_tx, reply_rx) = oneshot::channel();
                            let req = ToolCallRequest {
                                turn_id: current_turn_id,
                                call_id: call_id.clone(),
                                function_name: name.clone(),
                                args,
                                reply_tx,
                            };
                            let handler = handler.clone();
                            let outgoing = tx_outgoing_ws.clone();
                            let turn_id = current_turn_id;

                            tokio::spawn(async move {
                                let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                    handler.send(req).await.ok()?;
                                    reply_rx.await.ok()
                                }).await;

                                let output = match result {
                                    Ok(Some(v)) => v,
                                    _ => json!({"status": "timeout"}),
                                };

                                let item_msg = json!({
                                    "type": "conversation.item.create",
                                    "item": {
                                        "type": "function_call_output",
                                        "call_id": call_id,
                                        "output": output.to_string(),
                                    }
                                }).to_string();
                                let _ = outgoing.send((turn_id, item_msg)).await;

                                let resp_create = json!({
                                    "type": "response.create"
                                }).to_string();
                                let _ = outgoing.send((turn_id, resp_create)).await;
                            });
                        }
                    }
                    "error" => {
                        warn!("OpenAI Realtime error frame received: {:?}", resp.get("error"));
                    }
                    _ => {}
                }
            }
            _ = shutdown_rx.recv() => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_gemini_tool_to_openai_format() {
        let gemini_decl = json!({
            "name": "control_home_device",
            "description": "Smart home controls",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "domain": {"type": "string"},
                    "service": {"type": "string"}
                },
                "required": ["domain", "service"]
            }
        });

        let openai_tool = convert_gemini_tool_to_openai(gemini_decl);
        assert_eq!(openai_tool["type"], "function");
        assert_eq!(openai_tool["name"], "control_home_device");
        assert_eq!(openai_tool["description"], "Smart home controls");
        assert_eq!(openai_tool["parameters"]["type"], "object");
        assert_eq!(openai_tool["parameters"]["required"][0], "domain");
    }

    #[test]
    fn test_parse_openai_audio_delta_and_done() {
        let pcm_test = vec![10u8, 20, 30, 40];
        let b64 = BASE64.encode(&pcm_test);
        let delta_frame = json!({
            "type": "response.audio.delta",
            "delta": b64
        });

        let delta_type = delta_frame.get("type").and_then(|v| v.as_str()).unwrap();
        assert_eq!(delta_type, "response.audio.delta");
        let decoded = BASE64
            .decode(delta_frame["delta"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, pcm_test);
    }
}
