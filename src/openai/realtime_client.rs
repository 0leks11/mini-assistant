use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::gemini::{LiveClientConfig, SessionStatus, ToolCallRequest, UserAudio};
use crate::interaction::TurnAuthorization;

pub const OPENAI_REALTIME_URL: &str = "wss://api.openai.com/v1/realtime";

fn normalize_schema_types(val: &mut Value) {
    match val {
        Value::Object(map) => {
            if let Some(Value::String(ref mut ty)) = map.get_mut("type") {
                match ty.as_str() {
                    "OBJECT" | "STRING" | "INTEGER" | "NUMBER" | "BOOLEAN" | "ARRAY" => {
                        *ty = ty.to_ascii_lowercase();
                    }
                    _ => {}
                }
            }
            for v in map.values_mut() {
                normalize_schema_types(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_schema_types(v);
            }
        }
        _ => {}
    }
}

/// Convert a Gemini-style tool declaration to OpenAI function declaration.
pub fn convert_gemini_tool_to_openai(mut decl: Value) -> Value {
    let name = decl.get("name").cloned().unwrap_or(Value::Null);
    let desc = decl.get("description").cloned().unwrap_or(Value::Null);
    let mut params = decl
        .get_mut("parameters")
        .map(|p| p.take())
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

    normalize_schema_types(&mut params);

    json!({
        "type": "function",
        "name": name,
        "description": desc,
        "parameters": params,
    })
}

/// Streaming linear resampler from 16 kHz mono S16_LE PCM to 24 kHz mono S16_LE PCM (3/2 ratio).
/// OpenAI Realtime API `pcm16` expects 24,000 Hz audio.
#[derive(Debug, Default)]
pub struct Upsampler16To24 {
    prev: Option<i16>,
    phase_odd: bool,
}

impl Upsampler16To24 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.prev = None;
        self.phase_odd = false;
    }

    /// Process a 16 kHz PCM16 byte slice and return 24 kHz PCM16 bytes.
    pub fn process(&mut self, pcm_16k: &[u8]) -> Vec<u8> {
        let samples = pcm_16k.len() / 2;
        if samples == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity((samples * 3) + 2);
        for &pair in pcm_16k.as_chunks::<2>().0 {
            let curr = i16::from_le_bytes(pair);
            match self.prev {
                None => {
                    out.extend_from_slice(&curr.to_le_bytes());
                    self.prev = Some(curr);
                    self.phase_odd = true;
                }
                Some(prev) if self.phase_odd => {
                    let y = ((prev as i32 + 2 * curr as i32) / 3) as i16;
                    out.extend_from_slice(&y.to_le_bytes());
                    self.prev = Some(curr);
                    self.phase_odd = false;
                }
                Some(prev) => {
                    let y = ((2 * prev as i32 + curr as i32) / 3) as i16;
                    out.extend_from_slice(&y.to_le_bytes());
                    out.extend_from_slice(&curr.to_le_bytes());
                    self.prev = Some(curr);
                    self.phase_odd = true;
                }
            }
        }
        out
    }

    /// Emit any final interpolated tail sample at turn end and reset internal state.
    pub fn finish_turn(&mut self) -> Vec<u8> {
        let tail = if !self.phase_odd {
            self.prev
                .map(|s| s.to_le_bytes().to_vec())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.reset();
        tail
    }
}

fn sanitize_openai_error(resp: &Value) -> String {
    let err = resp.get("error");
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("unspecified");
    let ty = err
        .and_then(|e| e.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("error");
    let msg = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");
    let redacted = if msg.contains("Bearer ") || msg.contains("sk-") {
        "[message redacted]"
    } else if msg.len() > 160 {
        &msg[..160]
    } else {
        msg
    };
    format!("type={ty}, code={code}, message={redacted}")
}

fn indicates_dialogue_stop(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("молчу")
        || lower.contains("остановила")
        || lower.contains("остановил")
        || lower.contains("до свидания")
        || lower.contains("пока")
        || lower.contains("отбой")
}

#[derive(Debug)]
pub struct OpenAiRealtimeClient {
    status: Arc<AtomicU8>,
    shutdown_tx: broadcast::Sender<()>,
    supervisor_handle: Option<tokio::task::JoinHandle<()>>,
    authorization: Option<Arc<TurnAuthorization>>,
}

struct OpenAiTurnRecord {
    id: u64,
    cancelled_notified: bool,
    submitted_at: std::time::Instant,
}

#[derive(Default)]
struct OpenAiSessionTurns(Mutex<VecDeque<OpenAiTurnRecord>>);

impl OpenAiSessionTurns {
    fn submit(&self, id: u64) -> Result<()> {
        let mut turns = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Turn ledger unavailable"))?;
        anyhow::ensure!(id != 0, "Invalid turn id 0");
        let now = std::time::Instant::now();
        turns.retain(|r| now.duration_since(r.submitted_at) < std::time::Duration::from_secs(15));
        while turns.len() >= 8 {
            turns.pop_front();
        }
        if !turns.iter().any(|r| r.id == id) {
            turns.push_back(OpenAiTurnRecord {
                id,
                cancelled_notified: false,
                submitted_at: now,
            });
        }
        Ok(())
    }

    fn front(&self) -> Option<u64> {
        self.0.lock().ok()?.front().map(|r| r.id)
    }

    fn next_cancelled_to_notify(&self, auth: &TurnAuthorization) -> bool {
        if let Ok(mut turns) = self.0.lock() {
            let mut any = false;
            for r in turns.iter_mut() {
                if !r.cancelled_notified && !auth.is_current(r.id) {
                    r.cancelled_notified = true;
                    any = true;
                }
            }
            return any;
        }
        false
    }

    fn prune_finished(&self, auth: Option<&TurnAuthorization>) {
        if let Ok(mut turns) = self.0.lock() {
            if let Some(auth) = auth {
                turns.retain(|r| !auth.is_finished(r.id));
            }
            let now = std::time::Instant::now();
            turns.retain(|r| {
                now.duration_since(r.submitted_at) < std::time::Duration::from_secs(15)
            });
        }
    }

    fn acknowledge(&self, id: u64) {
        if let Ok(mut turns) = self.0.lock() {
            turns.retain(|r| r.id != id);
        }
    }
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
            let response_counter = Arc::new(AtomicU64::new(1));
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
                    response_counter.clone(),
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
                        backoff_secs = 1;
                        info!("OpenAI Realtime session closed normally, reconnecting in {backoff_secs}s...");
                    }
                    Err(e) => {
                        warn!("OpenAI Realtime session dropped ({e:#}), reconnecting in {backoff_secs}s...");
                    }
                }

                let sleep_fut = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs));
                tokio::pin!(sleep_fut);

                loop {
                    tokio::select! {
                        _ = &mut sleep_fut => {
                            backoff_secs = (backoff_secs * 2).min(10);
                            break;
                        }
                        _ = shutdown_rx.recv() => {
                            status_for_supervisor.store(SessionStatus::Stopped as u8, Ordering::SeqCst);
                            return;
                        }
                        maybe_stale = rx_user_audio.recv() => {
                            if maybe_stale.is_none() {
                                status_for_supervisor.store(SessionStatus::Stopped as u8, Ordering::SeqCst);
                                return;
                            }
                        }
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
                    authorization: cfg.authorization.clone(),
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
                                anyhow::bail!(
                                    "OpenAI Realtime setup rejected: {}",
                                    sanitize_openai_error(&resp)
                                );
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

        let _ = write.send(Message::Close(None)).await;
        Ok(())
    }

    pub async fn stop(&mut self) {
        if let Some(auth) = &self.authorization {
            auth.cancel();
        }
        let _ = self.shutdown_tx.send(());
        if let Some(h) = self.supervisor_handle.take() {
            if tokio::time::timeout(std::time::Duration::from_secs(2), h)
                .await
                .is_err()
            {
                debug!("OpenAI Realtime supervisor stop wait expired");
            }
        }
        self.status
            .store(SessionStatus::Stopped as u8, Ordering::SeqCst);
    }
}

fn build_openai_tools(cfg: &LiveClientConfig) -> Vec<Value> {
    let mut tools = Vec::new();
    if cfg.has_ha_tools {
        tools.push(json!({
            "type": "function",
            "name": "control_home_device",
            "description": "Управление устройствами умного дома в Home Assistant (свет, кондиционер Midea, выключатели, розетки, режимы и температура).",
            "parameters": {
                "type": "object",
                "properties": {
                    "domain": { "type": "string", "description": "Домен устройства ('climate', 'light', 'switch', 'media_player')" },
                    "service": { "type": "string", "description": "Команда ('turn_on', 'turn_off', 'set_temperature', 'set_hvac_mode')" },
                    "entity_id": { "type": "string", "description": "ID сущности Home Assistant" },
                    "parameters": { "type": "object", "description": "Дополнительные параметры (например {'temperature': 22})" }
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
                    "entity_id": { "type": "string", "description": "ID сущности в Home Assistant" },
                    "hours_ago": { "type": "integer", "description": "За сколько прошедших часов запросить историю (по умолчанию 24)" }
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
            "parameters": {
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "description": "Причина или слово остановки" }
                }
            }
        }));
    }
    tools
}

#[allow(clippy::too_many_arguments)]
async fn run_single_session(
    cfg: &LiveClientConfig,
    rx_user_audio: &mut mpsc::Receiver<UserAudio>,
    tx_model_audio: &mpsc::Sender<(u64, u64, Vec<u8>)>,
    tool_call_tx: &Option<mpsc::Sender<ToolCallRequest>>,
    on_interrupted: Arc<dyn Fn() + Send + Sync + 'static>,
    cancellation_epoch: Arc<AtomicU64>,
    response_counter: Arc<AtomicU64>,
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

    let tools = build_openai_tools(cfg);
    let voice = if cfg.voice_name.trim().is_empty() {
        "alloy"
    } else {
        cfg.voice_name.as_str()
    };

    let mut session_obj = json!({
        "modalities": ["text", "audio"],
        "instructions": cfg.system_instruction,
        "voice": voice,
        "input_audio_format": "pcm16",
        "output_audio_format": "pcm16",
        "turn_detection": null,
        "temperature": 0.7
    });
    if !tools.is_empty() {
        if let Some(obj) = session_obj.as_object_mut() {
            obj.insert("tools".to_string(), Value::Array(tools));
            obj.insert("tool_choice".to_string(), json!("auto"));
        }
    }

    let setup_msg = json!({
        "type": "session.update",
        "session": session_obj
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
                            anyhow::bail!(
                                "OpenAI Realtime setup rejected: {}",
                                sanitize_openai_error(&resp)
                            );
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
    let turns = Arc::new(OpenAiSessionTurns::default());

    let sender = run_sender_task(
        write,
        rx_user_audio,
        rx_outgoing_ws,
        cfg.authorization.clone(),
        turns.clone(),
        on_interrupted.clone(),
        shutdown_tx.subscribe(),
    );

    let receiver = run_receiver_task(
        read,
        tx_model_audio.clone(),
        tool_call_tx.clone(),
        tx_outgoing_ws,
        on_interrupted,
        cancellation_epoch,
        response_counter,
        cfg.authorization.clone(),
        turns,
        cfg.suppress_followup.clone(),
        shutdown_tx.subscribe(),
    );

    tokio::select! {
        res = sender => res,
        res = receiver => res,
    }
}

async fn cancel_openai_turn<S>(
    write: &mut S,
    upsampler: &mut Upsampler16To24,
    submitted_turn: &mut Option<u64>,
    turns: &OpenAiSessionTurns,
    authorization: Option<&TurnAuthorization>,
    on_interrupted: &Arc<dyn Fn() + Send + Sync + 'static>,
) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let had_submitted = submitted_turn.take().is_some();
    let had_wire_cancelled = authorization
        .map(|a| turns.next_cancelled_to_notify(a))
        .unwrap_or(false);
    if had_submitted || had_wire_cancelled {
        upsampler.reset();
        on_interrupted();
        write
            .send(Message::Text(
                json!({ "type": "input_audio_buffer.clear" }).to_string(),
            ))
            .await?;
        write
            .send(Message::Text(
                json!({ "type": "response.cancel" }).to_string(),
            ))
            .await?;
    }
    Ok(())
}

async fn run_sender_task<S>(
    mut write: S,
    rx_user: &mut mpsc::Receiver<UserAudio>,
    mut rx_outgoing_ws: mpsc::Receiver<(u64, String)>,
    authorization: Option<Arc<TurnAuthorization>>,
    turns: Arc<OpenAiSessionTurns>,
    on_interrupted: Arc<dyn Fn() + Send + Sync + 'static>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(30));
    ping.tick().await;
    let mut cancellation_poll = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut submitted_turn = None;
    let mut upsampler = Upsampler16To24::new();

    loop {
        tokio::select! {
            _ = ping.tick() => {
                write.send(Message::Ping(Vec::new())).await?;
            }
            _ = cancellation_poll.tick() => {
                turns.prune_finished(authorization.as_deref());
                let input_cancelled = submitted_turn.is_some_and(|id| !authorization.as_ref().is_some_and(|a| a.is_current(id)));
                let wire_cancelled = authorization.as_deref().is_some_and(|a| turns.next_cancelled_to_notify(a));
                if input_cancelled || wire_cancelled {
                    if input_cancelled {
                        submitted_turn = None;
                    }
                    upsampler.reset();
                    on_interrupted();
                    write.send(Message::Text(json!({ "type": "input_audio_buffer.clear" }).to_string())).await?;
                    write.send(Message::Text(json!({ "type": "response.cancel" }).to_string())).await?;
                }
            }
            maybe = rx_user.recv() => {
                let Some(input) = maybe else { return Ok(()) };
                if submitted_turn.is_some_and(|id| !authorization.as_ref().is_some_and(|a| a.is_current(id))) {
                    cancel_openai_turn(&mut write, &mut upsampler, &mut submitted_turn, &turns, authorization.as_deref(), &on_interrupted).await?;
                }
                let Some(auth) = &authorization else { continue };
                if !auth.is_current(input.turn_id) { continue; }

                if input.pcm.is_empty() {
                    if submitted_turn == Some(input.turn_id) {
                        let tail_24k = upsampler.finish_turn();
                        if !tail_24k.is_empty() {
                            write.send(Message::Text(json!({
                                "type": "input_audio_buffer.append",
                                "audio": BASE64.encode(&tail_24k)
                            }).to_string())).await?;
                        }
                        submitted_turn = None;
                        if auth.complete_input(input.turn_id) {
                            turns.submit(input.turn_id)?;
                            write.send(Message::Text(json!({
                                "type": "input_audio_buffer.commit"
                            }).to_string())).await?;
                            write.send(Message::Text(json!({
                                "type": "response.create"
                            }).to_string())).await?;
                        }
                    }
                    continue;
                }

                if !auth.mark_submitted(input.turn_id) {
                    continue;
                }
                if submitted_turn != Some(input.turn_id) {
                    cancel_openai_turn(&mut write, &mut upsampler, &mut submitted_turn, &turns, Some(auth.as_ref()), &on_interrupted).await?;
                    upsampler.reset();
                    submitted_turn = Some(input.turn_id);
                }

                let pcm_24k = upsampler.process(&input.pcm);
                if !pcm_24k.is_empty() {
                    let b64 = BASE64.encode(&pcm_24k);
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

#[allow(clippy::too_many_arguments)]
async fn run_receiver_task<S>(
    mut read: S,
    tx_model_audio: mpsc::Sender<(u64, u64, Vec<u8>)>,
    tool_call_tx: Option<mpsc::Sender<ToolCallRequest>>,
    tx_outgoing_ws: mpsc::Sender<(u64, String)>,
    on_interrupted: Arc<dyn Fn() + Send + Sync + 'static>,
    cancellation_epoch: Arc<AtomicU64>,
    response_counter: Arc<AtomicU64>,
    authorization: Option<Arc<TurnAuthorization>>,
    turns: Arc<OpenAiSessionTurns>,
    suppress_followup: Option<Arc<AtomicBool>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    S: futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    let mut response_id = response_counter.load(Ordering::SeqCst);
    let mut response_epoch = cancellation_epoch.load(Ordering::SeqCst);
    let mut response_turn: Option<u64> = None;
    let mut discard_generation = false;
    let mut emitted_audio = false;
    let mut audio_ended = false;
    let mut had_tool_call = false;

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
                        emitted_audio = false;
                        audio_ended = false;
                        had_tool_call = false;
                        if response_turn.is_none() {
                            turns.prune_finished(authorization.as_deref());
                            response_turn = turns.front();
                            response_epoch = cancellation_epoch.load(Ordering::SeqCst);
                            discard_generation = match response_turn {
                                Some(id) => !authorization
                                    .as_ref()
                                    .is_some_and(|a| a.accepting_response() == Some(id)),
                                None => true,
                            };
                        } else {
                            response_epoch = cancellation_epoch.load(Ordering::SeqCst);
                        }
                    }
                    "response.audio.delta" | "response.output_audio.delta" => {
                        let permitted = response_turn.filter(|id| {
                            !discard_generation
                                && authorization
                                    .as_ref()
                                    .is_some_and(|a| a.accepting_response() == Some(*id))
                        });
                        if permitted.is_some() {
                            if let Some(delta) = resp.get("delta").and_then(|d| d.as_str()) {
                                if let Ok(pcm_bytes) = BASE64.decode(delta) {
                                    if !pcm_bytes.is_empty() {
                                        emitted_audio = true;
                                        tx_model_audio
                                            .send((response_epoch, response_id, pcm_bytes))
                                            .await
                                            .context("Output closed")?;
                                    }
                                }
                            }
                        }
                    }
                    "response.audio_transcript.delta"
                    | "response.output_audio_transcript.delta"
                    | "response.audio_transcript.done"
                    | "response.output_audio_transcript.done"
                    | "response.text.delta" => {
                        let permitted = response_turn.is_some_and(|id| {
                            !discard_generation
                                && authorization
                                    .as_ref()
                                    .is_some_and(|a| a.accepting_response() == Some(id))
                        });
                        if permitted {
                            let fragment = resp
                                .get("delta")
                                .or_else(|| resp.get("transcript"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if !fragment.is_empty() && indicates_dialogue_stop(fragment) {
                                if let Some(ref suppress) = suppress_followup {
                                    suppress.store(true, Ordering::SeqCst);
                                }
                            }
                        }
                    }
                    "response.audio.done" | "response.output_audio.done" => {
                        let permitted = response_turn.is_some_and(|id| {
                            !discard_generation
                                && authorization
                                    .as_ref()
                                    .is_some_and(|a| a.accepting_response() == Some(id))
                        });
                        if permitted && emitted_audio && !audio_ended {
                            audio_ended = true;
                            tx_model_audio
                                .send((response_epoch, response_id, Vec::new()))
                                .await
                                .context("Output closed")?;
                        }
                    }
                    "response.function_call_arguments.done" => {
                        let Some(turn_id) = response_turn.filter(|id| {
                            !discard_generation
                                && authorization
                                    .as_ref()
                                    .is_some_and(|a| a.accepting_response() == Some(*id))
                        }) else {
                            continue;
                        };
                        had_tool_call = true;
                        let call_id = resp.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = resp.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let arguments_str = resp.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                        let args: Value = serde_json::from_str(arguments_str).unwrap_or(json!({}));

                        if let Some(ref handler) = tool_call_tx {
                            let (reply_tx, reply_rx) = oneshot::channel();
                            let req = ToolCallRequest {
                                turn_id,
                                call_id: call_id.clone(),
                                function_name: name,
                                args,
                                reply_tx,
                            };
                            let handler = handler.clone();
                            let outgoing = tx_outgoing_ws.clone();
                            let auth = authorization.clone();

                            tokio::spawn(async move {
                                if !auth.as_ref().is_some_and(|a| a.is_current(turn_id)) {
                                    return;
                                }
                                let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                    handler.send(req).await.ok()?;
                                    reply_rx.await.ok()
                                }).await;
                                if !auth.as_ref().is_some_and(|a| a.is_current(turn_id)) {
                                    return;
                                }

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
                    "response.done" => {
                        let status = resp
                            .pointer("/response/status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("completed");
                        let output_has_fn = resp
                            .pointer("/response/output")
                            .and_then(|v| v.as_array())
                            .is_some_and(|items| {
                                items.iter().any(|it| it.get("type").and_then(|t| t.as_str()) == Some("function_call"))
                            });
                        if status == "cancelled" {
                            if let Some(id) = response_turn.take() {
                                turns.acknowledge(id);
                            }
                            on_interrupted();
                            response_id = response_counter.fetch_add(1, Ordering::SeqCst) + 1;
                            response_epoch = cancellation_epoch.load(Ordering::SeqCst);
                            discard_generation = false;
                        } else if (had_tool_call || output_has_fn) && !discard_generation {
                            // Keep turn alive until follow-up response after tool outputs finishes.
                            if emitted_audio && !audio_ended {
                                audio_ended = true;
                                let _ = tx_model_audio
                                    .send((response_epoch, response_id, Vec::new()))
                                    .await;
                            }
                            response_id = response_counter.fetch_add(1, Ordering::SeqCst) + 1;
                        } else {
                            if let Some(tid) = response_turn.take() {
                                if !discard_generation
                                    && emitted_audio
                                    && !audio_ended
                                    && authorization
                                        .as_ref()
                                        .is_some_and(|a| a.accepting_response() == Some(tid))
                                {
                                    tx_model_audio
                                        .send((response_epoch, response_id, Vec::new()))
                                        .await
                                        .context("Output closed")?;
                                }
                                if let Some(auth) = &authorization {
                                    auth.complete_response(tid);
                                }
                                turns.acknowledge(tid);
                            }
                            response_id = response_counter.fetch_add(1, Ordering::SeqCst) + 1;
                            response_epoch = cancellation_epoch.load(Ordering::SeqCst);
                            discard_generation = false;
                        }
                    }
                    "error" => {
                        warn!("OpenAI Realtime error frame received: {}", sanitize_openai_error(&resp));
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
    use std::time::Duration;
    use tokio_tungstenite::accept_async;

    #[test]
    fn test_convert_gemini_tool_to_openai_format_recursive() {
        let weather_tool =
            convert_gemini_tool_to_openai(crate::weather::WeatherService::tool_declaration());
        assert_eq!(weather_tool["type"], "function");
        assert_eq!(weather_tool["parameters"]["type"], "object");
        assert_eq!(
            weather_tool["parameters"]["properties"]["location_id"]["type"],
            "string"
        );

        for alarm_decl in crate::alarms::alarm_tool_declarations() {
            let converted = convert_gemini_tool_to_openai(alarm_decl);
            let serialized = converted.to_string();
            assert!(!serialized.contains("\"OBJECT\""));
            assert!(!serialized.contains("\"STRING\""));
            assert!(!serialized.contains("\"ARRAY\""));
            assert!(!serialized.contains("\"INTEGER\""));
        }

        for sp_decl in crate::spotify::spotify_tool_declarations() {
            let converted = convert_gemini_tool_to_openai(sp_decl);
            let serialized = converted.to_string();
            assert!(!serialized.contains("\"OBJECT\""));
            assert!(!serialized.contains("\"STRING\""));
            assert!(!serialized.contains("\"INTEGER\""));
        }
    }

    #[test]
    fn test_upsampler_16k_to_24k_exact_ratio_and_continuity() {
        let mut upsampler = Upsampler16To24::new();

        // Linear ramp x[n] = 300 * n at 16 kHz -> output samples must equal 200 * k at 24 kHz.
        let mut chunk1 = Vec::new();
        for n in 0..256i32 {
            chunk1.extend_from_slice(&((n * 30) as i16).to_le_bytes());
        }
        let mut chunk2 = Vec::new();
        for n in 256..512i32 {
            chunk2.extend_from_slice(&((n * 30) as i16).to_le_bytes());
        }

        let out1 = upsampler.process(&chunk1);
        let out2 = upsampler.process(&chunk2);
        let tail = upsampler.finish_turn();

        let total_samples = (out1.len() + out2.len() + tail.len()) / 2;
        assert_eq!(
            total_samples, 768,
            "512 samples @ 16kHz must produce 768 samples @ 24kHz"
        );

        let mut all_samples = Vec::with_capacity(total_samples);
        for &bytes in out1
            .as_chunks::<2>()
            .0
            .iter()
            .chain(out2.as_chunks::<2>().0.iter())
        {
            all_samples.push(i16::from_le_bytes(bytes));
        }
        // First 767 interpolated samples must follow exact 20*k linear progression
        for (k, &val) in all_samples.iter().enumerate() {
            assert_eq!(val, (k as i32 * 20) as i16, "mismatch at sample {k}");
        }
    }

    #[test]
    fn test_build_openai_tools_matches_dispatcher_contract() {
        let cfg = LiveClientConfig {
            api_key: "sk-test".into(),
            model: "gpt-4o-realtime-preview".into(),
            voice_name: "alloy".into(),
            system_instruction: "test".into(),
            has_ha_tools: true,
            has_weather_tools: true,
            has_alarm_tools: true,
            has_spotify_tools: true,
            has_volume_tools: true,
            has_bluetooth_tools: true,
            has_stop_dialogue: true,
            endpoint: None,
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };
        let tools = build_openai_tools(&cfg);
        let ha_ctrl = tools
            .iter()
            .find(|t| t["name"] == "control_home_device")
            .expect("HA control tool present");
        assert!(
            ha_ctrl["parameters"]["properties"]["parameters"].is_object(),
            "control_home_device must use 'parameters' key expected by dispatcher"
        );
        let ha_hist = tools
            .iter()
            .find(|t| t["name"] == "get_device_history")
            .expect("HA history tool present");
        assert!(
            ha_hist["parameters"]["properties"]["hours_ago"].is_object(),
            "get_device_history must use 'hours_ago' key expected by dispatcher"
        );
        assert!(tools.iter().any(|t| t["name"] == "stop_dialogue"));
        assert!(tools.iter().any(|t| t["name"] == "spotify_play"));
    }

    #[test]
    fn test_sanitize_openai_error_redacts_secrets() {
        let resp = json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "code": "invalid_api_key",
                "message": "Incorrect API key provided: sk-secret-value-here"
            }
        });
        let s = sanitize_openai_error(&resp);
        assert!(s.contains("invalid_api_key"));
        assert!(!s.contains("sk-secret-value-here"));
    }

    #[tokio::test]
    async fn test_openai_client_turn_tool_and_barge_in_lifecycle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("ws://{addr}/v1/realtime");

        let server_events = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_events_log = server_events.clone();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            // Expect session.update
            let setup_frame = ws.next().await.unwrap().unwrap();
            let setup_json: Value = serde_json::from_str(setup_frame.to_text().unwrap()).unwrap();
            assert_eq!(setup_json["type"], "session.update");
            assert_eq!(setup_json["session"]["input_audio_format"], "pcm16");

            ws.send(Message::Text(
                json!({"type": "session.updated"}).to_string(),
            ))
            .await
            .unwrap();

            let mut committed = false;
            let mut saw_tool_output = false;
            while let Some(Ok(msg)) = ws.next().await {
                if let Ok(txt) = msg.into_text() {
                    if let Ok(val) = serde_json::from_str::<Value>(&txt) {
                        let ty = val["type"].as_str().unwrap_or("").to_string();
                        server_events_log.lock().unwrap().push(ty.clone());
                        if ty == "input_audio_buffer.append" {
                            let raw = BASE64.decode(val["audio"].as_str().unwrap()).unwrap();
                            assert!(!raw.is_empty());
                        } else if ty == "response.create" && !committed {
                            committed = true;
                            ws.send(Message::Text(
                                json!({"type": "response.created"}).to_string(),
                            ))
                            .await
                            .unwrap();
                            ws.send(Message::Text(
                                json!({
                                    "type": "response.function_call_arguments.done",
                                    "call_id": "call_1",
                                    "name": "set_volume",
                                    "arguments": "{\"level\":4}"
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                            ws.send(Message::Text(
                                json!({
                                    "type": "response.done",
                                    "response": { "status": "completed", "output": [{"type": "function_call"}] }
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                        } else if ty == "conversation.item.create" {
                            saw_tool_output = true;
                        } else if ty == "response.create" && saw_tool_output {
                            ws.send(Message::Text(
                                json!({"type": "response.created"}).to_string(),
                            ))
                            .await
                            .unwrap();
                            ws.send(Message::Text(
                                json!({
                                    "type": "response.audio.delta",
                                    "delta": BASE64.encode(vec![1u8, 2, 3, 4])
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                            ws.send(Message::Text(
                                json!({
                                    "type": "response.done",
                                    "response": { "status": "completed", "output": [{"type": "message"}] }
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                        }
                    }
                }
            }
        });

        let auth = Arc::new(TurnAuthorization::new(Duration::from_secs(10)));
        let epoch = Arc::new(AtomicU64::new(0));
        let (tx_user, rx_user) = mpsc::channel(16);
        let (tx_model, mut rx_model) = mpsc::channel(16);
        let (tx_tools, mut rx_tools) = mpsc::channel::<ToolCallRequest>(8);
        let interrupted_count = Arc::new(AtomicU64::new(0));
        let interrupted_cb = interrupted_count.clone();
        let epoch_cb = epoch.clone();
        let on_interrupted = Arc::new(move || {
            interrupted_cb.fetch_add(1, Ordering::SeqCst);
            epoch_cb.fetch_add(1, Ordering::SeqCst);
        });

        let tool_worker = tokio::spawn(async move {
            if let Some(req) = rx_tools.recv().await {
                assert_eq!(req.function_name, "set_volume");
                let _ = req.reply_tx.send(json!({"status": "ok", "level": 4}));
            }
        });

        let cfg = LiveClientConfig {
            api_key: "sk-test".into(),
            model: "gpt-4o-realtime-preview".into(),
            voice_name: "alloy".into(),
            system_instruction: "test".into(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: true,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(endpoint),
            cancellation_epoch: Some(epoch),
            authorization: Some(auth.clone()),
            suppress_followup: None,
        };

        let mut client =
            OpenAiRealtimeClient::connect(cfg, rx_user, tx_model, Some(tx_tools), on_interrupted)
                .await
                .unwrap();

        let tid = auth.begin_explicit_turn();
        tx_user
            .send(UserAudio {
                turn_id: tid,
                pcm: vec![0u8; 512],
            })
            .await
            .unwrap();
        tx_user
            .send(UserAudio {
                turn_id: tid,
                pcm: Vec::new(),
            })
            .await
            .unwrap();

        let (_ep, resp_id, pcm) = tokio::time::timeout(Duration::from_secs(3), rx_model.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pcm, vec![1, 2, 3, 4]);
        assert!(resp_id >= 2, "follow-up response must advance response_id");

        let (_ep2, resp_id2, end_pcm) =
            tokio::time::timeout(Duration::from_secs(3), rx_model.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(end_pcm.is_empty());
        assert_eq!(resp_id2, resp_id);

        // Verify barge-in cancel triggers buffer.clear and response.cancel
        let tid2 = auth.begin_explicit_turn();
        tx_user
            .send(UserAudio {
                turn_id: tid2,
                pcm: vec![1u8; 64],
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        auth.cancel();
        tokio::time::sleep(Duration::from_millis(200)).await;

        client.stop().await;
        server.abort();
        tool_worker.abort();

        let recorded = server_events.lock().unwrap();
        assert!(recorded.contains(&"conversation.item.create".to_string()));
        assert!(recorded.contains(&"response.cancel".to_string()));
        assert!(recorded.contains(&"input_audio_buffer.clear".to_string()));
        assert!(interrupted_count.load(Ordering::SeqCst) >= 1);
    }
}
