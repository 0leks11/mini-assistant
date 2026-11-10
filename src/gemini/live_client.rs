use crate::interaction::TurnAuthorization;
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

const GEMINI_LIVE_URL: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerResponse {
    #[serde(rename = "setupComplete", alias = "setup_complete")]
    pub setup_complete: Option<Value>,
    #[serde(rename = "serverContent", alias = "server_content")]
    pub server_content: Option<ServerContent>,
    #[serde(rename = "toolCall", alias = "tool_call")]
    pub tool_call: Option<ToolCall>,
    pub error: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerContent {
    #[serde(rename = "modelTurn", alias = "model_turn")]
    pub model_turn: Option<ModelTurn>,
    #[serde(rename = "turnComplete", alias = "turn_complete")]
    pub turn_complete: Option<bool>,
    #[serde(rename = "generationComplete", alias = "generation_complete")]
    pub generation_complete: Option<bool>,
    pub interrupted: Option<bool>,
    #[serde(rename = "outputTranscription", alias = "output_transcription")]
    pub output_transcription: Option<Transcription>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Transcription {
    pub text: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelTurn {
    pub parts: Option<Vec<Part>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Part {
    pub text: Option<String>,
    #[serde(rename = "inlineData", alias = "inline_data")]
    pub inline_data: Option<InlineData>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InlineData {
    #[serde(rename = "mimeType", alias = "mime_type")]
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "functionCalls", alias = "function_calls")]
    pub function_calls: Vec<FunctionCall>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

fn sanitize_setup_error(err_val: &Value) -> String {
    let code = err_val.get("code").and_then(|c| c.as_i64());
    let status = err_val.get("status").and_then(|s| s.as_str());
    match (code, status) {
        (Some(c), Some(s)) => format!("code={}, status={}", c, s),
        (Some(c), None) => format!("code={}", c),
        (None, Some(s)) => format!("status={}", s),
        _ => "unspecified".to_string(),
    }
}

/// Both WebSocket text and binary frames can contain the documented JSON envelope.
fn parse_server_frame(frame: &Message) -> Option<serde_json::Result<ServerResponse>> {
    let payload = match frame {
        Message::Text(text) => text.as_bytes(),
        Message::Binary(bytes) => bytes.as_slice(),
        _ => return None,
    };
    Some(serde_json::from_slice(payload))
}

pub struct UserAudio {
    pub turn_id: u64,
    pub pcm: Vec<u8>,
}

pub struct ToolCallRequest {
    pub turn_id: u64,
    #[allow(dead_code)]
    pub call_id: String,
    pub function_name: String,
    pub args: Value,
    pub reply_tx: oneshot::Sender<Value>,
}

#[derive(Debug, Clone)]
pub struct LiveClientConfig {
    pub api_key: String,
    pub model: String,
    pub voice_name: String,
    pub system_instruction: String,
    pub has_ha_tools: bool,
    pub has_weather_tools: bool,
    pub has_alarm_tools: bool,
    pub has_spotify_tools: bool,
    pub has_volume_tools: bool,
    pub has_bluetooth_tools: bool,
    pub has_stop_dialogue: bool,
    pub endpoint: Option<String>,
    /// A shared renderer epoch is advanced only by its interruption callback.
    pub cancellation_epoch: Option<Arc<AtomicU64>>,
    pub authorization: Option<Arc<TurnAuthorization>>,
    pub suppress_followup: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Connecting = 0,
    Ready = 1,
    Degraded = 2,
    Stopped = 3,
}

impl From<u8> for SessionStatus {
    fn from(v: u8) -> Self {
        match v {
            0 => SessionStatus::Connecting,
            1 => SessionStatus::Ready,
            2 => SessionStatus::Degraded,
            _ => SessionStatus::Stopped,
        }
    }
}

#[derive(Debug)]
pub struct GeminiLiveClient {
    #[allow(dead_code)]
    status: Arc<AtomicU8>,
    shutdown_tx: broadcast::Sender<()>,
    supervisor_handle: Option<tokio::task::JoinHandle<()>>,
    authorization: Option<Arc<TurnAuthorization>>,
}

impl GeminiLiveClient {
    #[allow(dead_code)]
    pub fn status(&self) -> SessionStatus {
        SessionStatus::from(self.status.load(Ordering::SeqCst))
    }

    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.status() == SessionStatus::Ready
    }

    /// Shared session status for the local microphone admission gate.
    /// Callers observe it only; the session supervisor owns transitions.
    #[allow(dead_code)]
    pub(crate) fn status_handle(&self) -> Arc<AtomicU8> {
        self.status.clone()
    }

    pub async fn stop(&mut self) {
        if let Some(auth) = &self.authorization {
            auth.cancel();
        }
        let _ = self.shutdown_tx.send(());
        if let Some(handle) = self.supervisor_handle.take() {
            let _ = handle.await;
        }
    }

    /// Perform a lightweight, non-streaming preflight handshake to verify API key and model setup.
    /// Closes the connection immediately upon receiving `setupComplete` without capturing or streaming audio.
    pub async fn verify_handshake(cfg: &LiveClientConfig) -> Result<()> {
        let url = match &cfg.endpoint {
            Some(custom) => custom.clone(),
            None => format!("{}?key={}", GEMINI_LIVE_URL, cfg.api_key),
        };

        let (ws_stream, _) = match connect_async(&url).await {
            Ok(res) => res,
            Err(e) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if now < 1_767_225_600 {
                    warn!(
                        unix_timestamp = now,
                        "Gemini Live WebSocket connect failed and system clock appears out of sync (< 2026)"
                    );
                }
                return Err(e).context("Failed to connect to Gemini Live WebSocket");
            }
        };

        let (mut write, mut read) = ws_stream.split();

        let setup_msg = json!({
            "setup": {
                "model": cfg.model,
                "generationConfig": {
                    "responseModalities": ["AUDIO"],
                    "speechConfig": {
                        "voiceConfig": {
                            "prebuiltVoiceConfig": {
                                "voiceName": cfg.voice_name
                            }
                        }
                    }
                }
            }
        });

        write
            .send(Message::Text(setup_msg.to_string()))
            .await
            .context("Failed to send setup message to Gemini Live")?;

        let setup_timeout = std::time::Duration::from_secs(10);
        tokio::time::timeout(setup_timeout, async {
            while let Some(msg_res) = read.next().await {
                let frame = msg_res.context("WebSocket error during setup handshake")?;
                match &frame {
                    Message::Text(text) => {
                        if let Ok(resp) = serde_json::from_str::<Value>(text) {
                            if resp.get("setupComplete").is_some() {
                                return Ok(());
                            }
                            if let Some(err) = resp.get("error") {
                                anyhow::bail!(
                                    "Gemini Live setup rejected ({})",
                                    sanitize_setup_error(err)
                                );
                            }
                        }
                    }
                    Message::Binary(bytes) => {
                        if let Ok(resp) = serde_json::from_slice::<Value>(bytes) {
                            if resp.get("setupComplete").is_some() {
                                return Ok(());
                            }
                            if let Some(err) = resp.get("error") {
                                anyhow::bail!(
                                    "Gemini Live setup rejected ({})",
                                    sanitize_setup_error(err)
                                );
                            }
                        }
                    }
                    Message::Close(frame) => {
                        anyhow::bail!("WebSocket closed during setup: {:?}", frame);
                    }
                    _ => {}
                }
            }
            anyhow::bail!("WebSocket stream ended before setup confirmation");
        })
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for setupComplete from Gemini Live"))??;

        let _ = write.send(Message::Close(None)).await;
        Ok(())
    }

    /// Connect to Gemini 2.0 Live API over WebSocket with resilient session supervision and auto-reconnect.
    pub async fn connect(
        cfg: LiveClientConfig,
        mut rx_user_audio: mpsc::Receiver<UserAudio>,
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
            // When playback shares the epoch, only its callback may advance
            // it. A local alarm can deliberately decline a voice interrupt.
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
                        Ok(()) => anyhow::anyhow!("Session closed before setup confirmation"),
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
                        info!("Gemini Live session closed normally, reconnecting in {backoff_secs}s...");
                    }
                    Err(e) => {
                        warn!("Gemini Live session dropped ({e:#}), reconnecting in {backoff_secs}s...");
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

        let initial_res = initial_rx
            .await
            .map_err(|_| anyhow::anyhow!("Supervisor task aborted during initial connection"))?;
        initial_res?;

        Ok(Self {
            status,
            shutdown_tx,
            supervisor_handle: Some(supervisor_handle),
            authorization: cfg.authorization.clone(),
        })
    }
}

/// Wire activity boundaries preserve the identity of cancelled inputs until the
/// server acknowledges their boundary. A new local permit cannot rename them.
#[derive(Default)]
struct SessionTurns(Mutex<VecDeque<u64>>);

impl SessionTurns {
    fn submit(&self, id: u64) -> Result<()> {
        let mut turns = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Turn ledger unavailable"))?;
        anyhow::ensure!(
            id != 0 && turns.len() < 8,
            "Too many unacknowledged input turns"
        );
        anyhow::ensure!(!turns.contains(&id), "Input turn was already submitted");
        turns.push_back(id);
        Ok(())
    }

    fn front(&self) -> Option<u64> {
        self.0.lock().ok()?.front().copied()
    }

    fn acknowledge(&self, id: u64) {
        if let Ok(mut turns) = self.0.lock() {
            if turns.front() == Some(&id) {
                turns.pop_front();
            }
        }
    }
}

async fn end_wire_activity<S>(
    write: &mut S,
    turns: &SessionTurns,
    submitted_turn: &mut Option<u64>,
) -> Result<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    if let Some(id) = submitted_turn.take() {
        // Record before the write: a server can answer immediately after activityEnd.
        // A cancelled ID remains in the ledger only to discard its eventual response.
        turns.submit(id)?;
        write
            .send(Message::Text(
                json!({"realtimeInput":{"activityEnd":{}}}).to_string(),
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
    turns: Arc<SessionTurns>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut submitted_turn = None;
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(30));
    ping.tick().await;
    let mut cancellation_poll = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = ping.tick() => write.send(Message::Ping(Vec::new())).await?,
            _ = cancellation_poll.tick() => {
                if submitted_turn.is_some_and(|id| !authorization.as_ref().is_some_and(|a| a.is_current(id))) {
                    end_wire_activity(&mut write, &turns, &mut submitted_turn).await?;
                }
            }
            maybe = rx_user.recv() => {
                let Some(input) = maybe else { return Ok(()) };
                if submitted_turn.is_some_and(|id| !authorization.as_ref().is_some_and(|a| a.is_current(id))) {
                    end_wire_activity(&mut write, &turns, &mut submitted_turn).await?;
                }
                let Some(auth) = &authorization else { continue };
                if !auth.is_current(input.turn_id) { continue; }
                if input.pcm.is_empty() {
                    if submitted_turn == Some(input.turn_id) {
                        // Merely receiving END must never authorize an empty input.
                        let _ = auth.complete_input(input.turn_id);
                        end_wire_activity(&mut write, &turns, &mut submitted_turn).await?;
                    }
                    continue;
                }
                // This also rejects all PCM received after the same input's END.
                if !auth.mark_submitted(input.turn_id) { continue; }
                if submitted_turn != Some(input.turn_id) {
                    end_wire_activity(&mut write, &turns, &mut submitted_turn).await?;
                    write.send(Message::Text(json!({"realtimeInput":{"activityStart":{}}}).to_string())).await?;
                    submitted_turn = Some(input.turn_id);
                }
                let data = BASE64.encode(input.pcm);
                write.send(Message::Text(json!({"realtimeInput":{"audio":{"mimeType":"audio/pcm;rate=16000","data":data}}}).to_string())).await?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolTurnState {
    Idle,
    AwaitingYield,
    AwaitingResult,
}

#[allow(clippy::too_many_arguments)]
async fn run_receiver_task<S>(
    mut read: S,
    tx_model_audio: mpsc::Sender<(u64, u64, Vec<u8>)>,
    tool_call_tx: Option<mpsc::Sender<ToolCallRequest>>,
    tx_outgoing_ws: mpsc::Sender<(u64, String)>,
    on_interrupted: Arc<impl Fn() + Send + Sync + 'static>,
    cancellation_epoch: Arc<AtomicU64>,
    response_counter: Arc<AtomicU64>,
    authorization: Option<Arc<TurnAuthorization>>,
    turns: Arc<SessionTurns>,
    suppress_followup: Option<Arc<AtomicBool>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    S: Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut response_complete = false;
    let mut response_id = response_counter.load(Ordering::SeqCst);
    let mut response_epoch = cancellation_epoch.load(Ordering::SeqCst);
    let mut response_turn = None;
    let mut discard_generation = false;
    let mut skip_unbound_completion_after_interrupt = false;
    let mut tool_turn_state = ToolTurnState::Idle;
    loop {
        let frame = tokio::select! {
            message = read.next() => match message { Some(m) => m?, None => return Ok(()) },
            _ = shutdown_rx.recv() => return Ok(()),
        };
        let Some(parsed) = parse_server_frame(&frame) else {
            if matches!(frame, Message::Close(_)) {
                return Ok(());
            }
            continue;
        };
        let Ok(resp) = parsed else {
            warn!("Invalid model envelope (payload omitted)");
            continue;
        };
        if resp.error.is_some() {
            warn!("Model error (details omitted)");
        }
        let interrupted = resp
            .server_content
            .as_ref()
            .is_some_and(|c| c.interrupted == Some(true));
        if interrupted {
            // If the old response had not started, the oldest wire input still
            // identifies it. Never use the newest local permit as its identity.
            let cancelled =
                response_turn.or_else(|| (!discard_generation).then(|| turns.front()).flatten());
            if let Some(id) = cancelled {
                turns.acknowledge(id);
            }
            on_interrupted();
            response_id = response_counter.fetch_add(1, Ordering::SeqCst) + 1;
            response_epoch = cancellation_epoch.load(Ordering::SeqCst);
            response_turn = None;
            response_complete = false;
            discard_generation = false;
            skip_unbound_completion_after_interrupt = true;
            tool_turn_state = ToolTurnState::Idle;
            continue;
        }
        let has_output = resp
            .server_content
            .as_ref()
            .is_some_and(|c| c.model_turn.is_some())
            || resp.tool_call.is_some();
        if has_output && response_turn.is_none() && !discard_generation {
            skip_unbound_completion_after_interrupt = false;
            if let Some(id) = turns.front() {
                if response_complete {
                    response_id = response_counter.fetch_add(1, Ordering::SeqCst) + 1;
                }
                response_complete = false;
                response_epoch = cancellation_epoch.load(Ordering::SeqCst);
                response_turn = Some(id);
                if !authorization
                    .as_ref()
                    .is_some_and(|a| a.accepting_response() == Some(id))
                {
                    discard_generation = true;
                }
            } else {
                // Keep dropping this entire unsolicited generation, even if a
                // real local activation happens before its remote completion.
                discard_generation = true;
            }
        }
        let permitted = response_turn.filter(|id| {
            !discard_generation
                && authorization
                    .as_ref()
                    .is_some_and(|a| a.accepting_response() == Some(*id))
        });
        if let Some(turn_id) = permitted {
            if let Some(turn) = resp
                .server_content
                .as_ref()
                .and_then(|c| c.model_turn.as_ref())
            {
                if tool_turn_state == ToolTurnState::AwaitingYield {
                    tool_turn_state = ToolTurnState::AwaitingResult;
                }
                for part in turn.parts.as_deref().unwrap_or_default() {
                    if let Some(text) = &part.text {
                        let lower = text.to_lowercase();
                        if lower.contains("молчу")
                            || lower.contains("остановила")
                            || lower.contains("остановил")
                            || lower.contains("до свидания")
                            || lower.contains("пока")
                            || lower.contains("отбой")
                        {
                            if let Some(ref suppress) = suppress_followup {
                                debug!("Model text indicates dialogue termination: suppressing follow-up");
                                suppress.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    if let Some(inline) = &part.inline_data {
                        if inline.mime_type.starts_with("audio/pcm") {
                            if let Ok(pcm) = BASE64.decode(&inline.data) {
                                if !pcm.is_empty() {
                                    tx_model_audio
                                        .send((response_epoch, response_id, pcm))
                                        .await
                                        .context("Output closed")?;
                                }
                            }
                        }
                    }
                }
            }
            if let (Some(tc), Some(handler)) = (resp.tool_call, &tool_call_tx) {
                tool_turn_state = ToolTurnState::AwaitingYield;
                for call in tc.function_calls {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let request = ToolCallRequest {
                        turn_id,
                        call_id: call.id.clone(),
                        function_name: call.name.clone(),
                        args: call.args,
                        reply_tx,
                    };
                    let handler = handler.clone();
                    let outgoing = tx_outgoing_ws.clone();
                    let auth = authorization.clone();
                    tokio::spawn(async move {
                        if !auth.as_ref().is_some_and(|a| a.is_current(turn_id)) {
                            return;
                        }
                        let result =
                            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                handler.send(request).await.ok()?;
                                reply_rx.await.ok()
                            })
                            .await;
                        if !auth.as_ref().is_some_and(|a| a.is_current(turn_id)) {
                            return;
                        }
                        let output = match result {
                            Ok(Some(v)) => v,
                            _ => json!({"status":"timeout"}),
                        };
                        let message = json!({"toolResponse":{"functionResponses":[{"id":call.id,"name":call.name,"response":output}]}}).to_string();
                        let _ = outgoing.send((turn_id, message)).await;
                    });
                }
            }
        }
        // generationComplete is not the end of the audio/input turn.
        if resp
            .server_content
            .as_ref()
            .is_some_and(|c| c.turn_complete == Some(true))
        {
            if tool_turn_state == ToolTurnState::AwaitingYield {
                // This turnComplete is Gemini yielding the turn while waiting for our tool responses.
                // The turn is NOT complete yet! Do not acknowledge turns, do not complete response, do not send Vec::new().
                tool_turn_state = ToolTurnState::AwaitingResult;
                continue;
            }
            tool_turn_state = ToolTurnState::Idle;
            if response_turn.is_none() && !discard_generation {
                if skip_unbound_completion_after_interrupt {
                    // This can acknowledge the interrupted generation. It must
                    // not consume the next input that has not produced output.
                    skip_unbound_completion_after_interrupt = false;
                    continue;
                }
                response_turn = turns.front();
                response_epoch = cancellation_epoch.load(Ordering::SeqCst);
            }
            if let Some(turn_id) = response_turn {
                if !discard_generation
                    && authorization
                        .as_ref()
                        .is_some_and(|a| a.accepting_response() == Some(turn_id))
                {
                    tx_model_audio
                        .send((response_epoch, response_id, Vec::new()))
                        .await
                        .context("Output closed")?;
                }
                if let Some(auth) = &authorization {
                    auth.complete_response(turn_id);
                }
                turns.acknowledge(turn_id);
            }
            response_complete = true;
            response_turn = None;
            discard_generation = false;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_single_session(
    cfg: &LiveClientConfig,
    rx_user_audio: &mut mpsc::Receiver<UserAudio>,
    tx_model_audio: &mpsc::Sender<(u64, u64, Vec<u8>)>,
    tool_call_tx: &Option<mpsc::Sender<ToolCallRequest>>,
    on_interrupted: Arc<impl Fn() + Send + Sync + 'static>,
    cancellation_epoch: Arc<AtomicU64>,
    response_counter: Arc<AtomicU64>,
    status: Arc<AtomicU8>,
    shutdown_tx: broadcast::Sender<()>,
    initial_tx: &mut Option<oneshot::Sender<Result<()>>>,
) -> Result<()> {
    let url = match &cfg.endpoint {
        Some(custom) => custom.clone(),
        None => format!("{}?key={}", GEMINI_LIVE_URL, cfg.api_key),
    };
    info!(
        "Connecting to Gemini Live API ({url_masked})...",
        url_masked = if cfg.endpoint.is_some() {
            "custom-endpoint"
        } else {
            "wss://.../BidiGenerateContent?key=***"
        }
    );

    let (ws_stream, _) = match connect_async(&url).await {
        Ok(res) => res,
        Err(e) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now < 1_767_225_600 {
                warn!(
                    unix_timestamp = now,
                    "Gemini Live WebSocket connect failed and system clock appears out of sync (< 2026)"
                );
            }
            return Err(e).context("Failed to connect to Gemini Live WebSocket");
        }
    };
    info!("Connected to Gemini Live API WebSocket successfully");

    let (mut write, mut read) = ws_stream.split();

    // 1. Build Tools Declaration for Home Assistant and Weather if enabled
    let mut function_declarations = Vec::new();
    if cfg.has_ha_tools {
        function_declarations.push(json!({
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
                        "description": "Действие: 'turn_on' (включить), 'turn_off' (выключить), 'set_temperature' (установить температуру), 'set_hvac_mode' (установить режим)"
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
        }));
        function_declarations.push(json!({
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
        }));
    }

    if cfg.has_weather_tools {
        function_declarations.push(crate::weather::WeatherService::tool_declaration());
    }

    if cfg.has_alarm_tools {
        function_declarations.extend(crate::alarms::alarm_tool_declarations());
    }

    if cfg.has_spotify_tools {
        function_declarations.extend(crate::spotify::spotify_tool_declarations());
    }

    if cfg.has_volume_tools {
        function_declarations.extend(vec![
            json!({
                "name": "set_volume",
                "description": "Установить громкость колонки от 0 до 10. 0 = тишина, 10 = максимум. Вызывай при фразах «громкость 5», «поставь громкость на 3», «сделай громкость 7».",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "level": {
                            "type": "INTEGER",
                            "description": "Уровень громкости от 0 до 10"
                        }
                    },
                    "required": ["level"]
                }
            }),
            json!({
                "name": "volume_up",
                "description": "Увеличить громкость на один шаг. Вызывай при фразах «громче», «погромче», «прибавь звук», «сделай громче».",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {}
                }
            }),
            json!({
                "name": "volume_down",
                "description": "Уменьшить громкость на один шаг. Вызывай при фразах «тише», «потише», «убавь звук», «сделай тише».",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {}
                }
            }),
        ]);
    }

    if cfg.has_bluetooth_tools {
        function_declarations.push(json!({
            "name": "enable_bluetooth",
            "description": "Включить Bluetooth для поиска и подключения телефона или ноутбука к колонке (режим сопряжения/обнаружения). Вызывай при фразах «включи блютуз», «включи bluetooth», «режим блютуз», «подключи по bluetooth».",
            "parameters": {
                "type": "OBJECT",
                "properties": {}
            }
        }));
    }

    if cfg.has_stop_dialogue {
        function_declarations.push(json!({
            "name": "stop_dialogue",
            "description": "Завершить диалог или остановить разговор/воспроизведение. Вызывай немедленно, если пользователь говорит «хватит», «все», «всё», «стоп», «молчи», «замолчи», «отбой», «пока», «до свидания», «достаточно», или явно просит замолчать/завершить сессию.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "reason": {
                        "type": "STRING",
                        "description": "Причина или слово остановки, например 'хватит' или 'все'"
                    }
                }
            }
        }));
    }

    let mut tools = Vec::new();
    if !function_declarations.is_empty() {
        tools.push(json!({
            "functionDeclarations": function_declarations
        }));
    }

    // 2. Build Setup Handshake Payload
    let mut setup_obj = json!({
        "model": cfg.model,
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": {
                "voiceConfig": {
                    "prebuiltVoiceConfig": {
                        "voiceName": cfg.voice_name
                    }
                }
            }
        },
        "realtimeInputConfig": {"automaticActivityDetection": {"disabled": true}},
        "systemInstruction": {
            "parts": [
                {
                    "text": cfg.system_instruction
                }
            ]
        }
    });

    if !tools.is_empty() {
        setup_obj
            .as_object_mut()
            .unwrap()
            .insert("tools".to_string(), Value::Array(tools));
    }

    let setup_msg = json!({ "setup": setup_obj });
    write
        .send(Message::Text(setup_msg.to_string()))
        .await
        .context("Failed to send setup message to Gemini Live")?;
    info!(
        "Sent setup configuration to Gemini Live (voice: {}, ha_tools: {}, weather_tools: {}, alarm_tools: {}, spotify_tools: {}, volume_tools: {}, bluetooth_tools: {})",
        cfg.voice_name, cfg.has_ha_tools, cfg.has_weather_tools, cfg.has_alarm_tools, cfg.has_spotify_tools, cfg.has_volume_tools, cfg.has_bluetooth_tools
    );

    // Wait for setupComplete confirmation from server before starting audio stream
    let setup_timeout = std::time::Duration::from_secs(10);
    let setup_res: Result<()> = tokio::time::timeout(setup_timeout, async {
        while let Some(msg_res) = read.next().await {
            let frame = msg_res.context("WebSocket error during setup handshake")?;
            match &frame {
                Message::Text(text) => {
                    if let Ok(resp) = serde_json::from_str::<Value>(text) {
                        if resp.get("setupComplete").is_some() {
                            return Ok(());
                        }
                        if let Some(err) = resp.get("error") {
                            anyhow::bail!(
                                "Gemini Live setup rejected ({})",
                                sanitize_setup_error(err)
                            );
                        }
                    }
                }
                Message::Binary(bytes) => {
                    if let Ok(resp) = serde_json::from_slice::<Value>(bytes) {
                        if resp.get("setupComplete").is_some() {
                            return Ok(());
                        }
                        if let Some(err) = resp.get("error") {
                            anyhow::bail!(
                                "Gemini Live setup rejected ({})",
                                sanitize_setup_error(err)
                            );
                        }
                    }
                }
                Message::Close(close_frame) => {
                    let code = close_frame
                        .as_ref()
                        .map(|cf| cf.code.into())
                        .unwrap_or(0u16);
                    anyhow::bail!(
                        "WebSocket closed by server during setup handshake (close_code={})",
                        code
                    );
                }
                _ => continue,
            }
        }
        anyhow::bail!("WebSocket stream closed before receiving setupComplete");
    })
    .await
    .context("Timed out waiting for Gemini Live setupComplete (10s)")?;

    setup_res?;
    info!("Gemini Live setup handshake completed successfully (setupComplete confirmed)");

    // A keyword heard while disconnected/handshaking must not replay after reconnect.
    if let Some(auth) = &cfg.authorization {
        auth.cancel();
    }
    while rx_user_audio.try_recv().is_ok() {}

    status.store(SessionStatus::Ready as u8, Ordering::SeqCst);
    if let Some(tx) = initial_tx.take() {
        let _ = tx.send(Ok(()));
    }

    let (tx_outgoing_ws, rx_outgoing_ws) = mpsc::channel::<(u64, String)>(50);

    let turns = Arc::new(SessionTurns::default());
    let sender = run_sender_task(
        write,
        rx_user_audio,
        rx_outgoing_ws,
        cfg.authorization.clone(),
        turns.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_documented_nested_interruption_text_frame() {
        let message = Message::Text(r#"{"serverContent":{"interrupted":true}}"#.into());
        let response = parse_server_frame(&message).unwrap().unwrap();
        assert_eq!(response.server_content.unwrap().interrupted, Some(true));
    }

    #[test]
    fn test_binary_json_envelope_matches_text() {
        let json = r#"{"serverContent":{"turnComplete":true,"modelTurn":{"parts":[{"inlineData":{"mimeType":"audio/pcm;rate=24000","data":"AAA="}}]}}}"#;
        for frame in [
            Message::Text(json.into()),
            Message::Binary(json.as_bytes().to_vec()),
        ] {
            let content = parse_server_frame(&frame)
                .unwrap()
                .unwrap()
                .server_content
                .unwrap();
            assert_eq!(content.turn_complete, Some(true));
            let inline = content
                .model_turn
                .unwrap()
                .parts
                .unwrap()
                .pop()
                .unwrap()
                .inline_data
                .unwrap();
            assert_eq!(BASE64.decode(inline.data).unwrap(), [0, 0]);
        }
    }

    #[test]
    fn test_invalid_json_and_control_frames() {
        assert!(parse_server_frame(&Message::Binary(vec![0xff]))
            .unwrap()
            .is_err());
        assert!(parse_server_frame(&Message::Ping(vec![])).is_none());
        assert!(parse_server_frame(&Message::Close(None)).is_none());
    }

    #[test]
    fn test_setup_complete_envelope() {
        let frame = Message::Text(r#"{"setupComplete":{}}"#.to_string());
        let resp = parse_server_frame(&frame).unwrap().unwrap();
        assert!(resp.setup_complete.is_some());
    }

    #[test]
    fn test_parse_tool_call_frame() {
        let json = r#"{"toolCall":{"functionCalls":[{"name":"control_home_device","args":{"domain":"climate","service":"turn_on"},"id":"call_123"}]}}"#;
        let resp = parse_server_frame(&Message::Text(json.into()))
            .unwrap()
            .unwrap();
        let tc = resp.tool_call.unwrap();
        assert_eq!(tc.function_calls.len(), 1);
        assert_eq!(tc.function_calls[0].name, "control_home_device");
        assert_eq!(tc.function_calls[0].id, "call_123");
    }

    #[test]
    fn test_parse_transcription_envelope() {
        let json = r#"{"serverContent":{"outputTranscription":{"text":"hello world"}}}"#;
        let resp = parse_server_frame(&Message::Text(json.into()))
            .unwrap()
            .unwrap();
        let content = resp.server_content.unwrap();
        let trans = content.output_transcription.unwrap();
        assert_eq!(trans.text.as_deref(), Some("hello world"));
    }

    #[test]
    fn test_sanitize_setup_error_omits_sensitive_payloads() {
        let err_json = json!({
            "code": 403,
            "status": "PERMISSION_DENIED",
            "message": "SECRET_BEARER_TOKEN_OR_USER_QUERY_SHOULD_NOT_LEAK",
            "details": ["private internal trace", 12345]
        });
        let sanitized = sanitize_setup_error(&err_json);
        assert_eq!(sanitized, "code=403, status=PERMISSION_DENIED");
        assert!(!sanitized.contains("SECRET_BEARER_TOKEN"));
        assert!(!sanitized.contains("private internal trace"));

        let empty_err = json!({"something_unexpected": "secret"});
        assert_eq!(sanitize_setup_error(&empty_err), "unspecified");
    }

    #[tokio::test]
    async fn test_supervisor_ready_reconnect_and_stop_lifecycle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{}/ws", port);

        let server_task = tokio::spawn(async move {
            // First session
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    let _ = ws.next().await; // Read setup
                    let resp = json!({ "setupComplete": {} });
                    let _ = ws.send(Message::Text(resp.to_string())).await;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let _ = ws.close(None).await;
                }
            }

            // Second session (reconnect)
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    let _ = ws.next().await; // Read setup
                    let resp = json!({ "setupComplete": {} });
                    let _ = ws.send(Message::Text(resp.to_string())).await;
                    while ws.next().await.is_some() {}
                }
            }
        });

        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "models/gemini-2.0-flash-exp".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(ws_url),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let interrupted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let interrupted_clone = interrupted.clone();
        let on_interrupted = Arc::new(move || {
            interrupted_clone.fetch_add(1, Ordering::SeqCst);
        });

        let mut client = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted)
            .await
            .expect("Initial connection must succeed");

        assert_eq!(client.status(), SessionStatus::Ready);
        assert!(client.is_ready());

        // Wait up to 3s for disconnect, degraded state, and reconnect back to Ready
        let mut reconnected = false;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if client.status() == SessionStatus::Ready && interrupted.load(Ordering::SeqCst) >= 1 {
                reconnected = true;
                break;
            }
        }
        assert!(
            reconnected,
            "Supervisor should have reconnected and reached Ready"
        );
        assert!(
            interrupted.load(Ordering::SeqCst) >= 1,
            "Interruption handler should be called on disconnect"
        );

        client.stop().await;
        assert_eq!(client.status(), SessionStatus::Stopped);

        let _ = server_task.await;
    }

    #[tokio::test]
    async fn test_supervisor_initial_connect_failure_fails_fast() {
        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "models/gemini-2.0-flash-exp".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            // Point to a non-listening port on loopback
            endpoint: Some("ws://127.0.0.1:1".to_string()),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let on_interrupted = Arc::new(|| {});

        let start = std::time::Instant::now();
        let res = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted).await;

        assert!(
            res.is_err(),
            "Initial connection to invalid endpoint must fail fast"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "Must not hang indefinitely"
        );
    }

    #[tokio::test]
    async fn test_supervisor_initial_setup_rejected_fails_fast() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{}/ws", port);

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    let _ = ws.next().await;
                    let err_resp = json!({
                        "error": {
                            "code": 400,
                            "status": "INVALID_ARGUMENT",
                            "message": "Model not supported"
                        }
                    });
                    let _ = ws.send(Message::Text(err_resp.to_string())).await;
                }
            }
        });

        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "bad-model".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(ws_url),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let on_interrupted = Arc::new(|| {});

        let res = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted).await;

        assert!(res.is_err(), "Setup error must fail initial connect");
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("Gemini Live setup rejected"));
    }

    #[tokio::test]
    async fn test_weather_tool_declaration_sent_without_ha() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{}/ws", port);

        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<Value>();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    if let Some(Ok(Message::Text(setup_text))) = ws.next().await {
                        let val: Value = serde_json::from_str(&setup_text).unwrap();
                        let _ = setup_tx.send(val);
                    }
                    let resp = json!({ "setupComplete": {} });
                    let _ = ws.send(Message::Text(resp.to_string())).await;
                    while ws.next().await.is_some() {}
                }
            }
        });

        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "models/gemini-2.0-flash-exp".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: true,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(ws_url),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let on_interrupted = Arc::new(|| {});

        let mut client = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted)
            .await
            .unwrap();

        let setup_val = setup_rx.await.unwrap();
        let tools = setup_val["setup"]["tools"]
            .as_array()
            .expect("tools must be present");
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "get_weather");

        client.stop().await;
    }

    #[tokio::test]
    async fn test_alarm_tool_declarations_sent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{}/ws", port);

        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<Value>();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    if let Some(Ok(Message::Text(setup_text))) = ws.next().await {
                        let val: Value = serde_json::from_str(&setup_text).unwrap();
                        let _ = setup_tx.send(val);
                    }
                    let resp = json!({ "setupComplete": {} });
                    let _ = ws.send(Message::Text(resp.to_string())).await;
                    while ws.next().await.is_some() {}
                }
            }
        });

        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "models/gemini-2.0-flash-exp".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: true,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(ws_url),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let on_interrupted = Arc::new(|| {});

        let mut client = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted)
            .await
            .unwrap();

        let setup_val = setup_rx.await.unwrap();
        let tools = setup_val["setup"]["tools"]
            .as_array()
            .expect("tools must be present");
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 5);
        let names: Vec<&str> = decls.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(names.contains(&"create_alarm"));
        assert!(names.contains(&"list_alarms"));
        assert!(names.contains(&"delete_alarm"));
        assert!(names.contains(&"snooze_alarm"));
        assert!(names.contains(&"stop_alarm"));

        client.stop().await;
    }

    #[tokio::test]
    async fn test_spotify_tool_declarations_sent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{}/ws", port);

        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<Value>();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    if let Some(Ok(Message::Text(setup_text))) = ws.next().await {
                        let val: Value = serde_json::from_str(&setup_text).unwrap();
                        let _ = setup_tx.send(val);
                    }
                    let resp = json!({ "setupComplete": {} });
                    let _ = ws.send(Message::Text(resp.to_string())).await;
                    while ws.next().await.is_some() {}
                }
            }
        });

        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "models/gemini-2.0-flash-exp".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: true,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(ws_url),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let on_interrupted = Arc::new(|| {});

        let mut client = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted)
            .await
            .unwrap();

        let setup_val = setup_rx.await.unwrap();
        let tools = setup_val["setup"]["tools"]
            .as_array()
            .expect("tools must be present");
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 6);
        let names: Vec<&str> = decls.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(names.contains(&"spotify_play"));
        assert!(names.contains(&"spotify_pause"));
        assert!(names.contains(&"spotify_next"));
        assert!(names.contains(&"spotify_previous"));
        assert!(names.contains(&"spotify_volume"));
        assert!(names.contains(&"spotify_current_track"));

        client.stop().await;
    }

    #[tokio::test]
    async fn test_stop_dialogue_tool_declarations_sent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{}/ws", port);

        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<Value>();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    if let Some(Ok(Message::Text(setup_text))) = ws.next().await {
                        let val: Value = serde_json::from_str(&setup_text).unwrap();
                        let _ = setup_tx.send(val);
                    }
                    let resp = json!({ "setupComplete": {} });
                    let _ = ws.send(Message::Text(resp.to_string())).await;
                    while ws.next().await.is_some() {}
                }
            }
        });

        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "models/gemini-2.0-flash-exp".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: true,
            endpoint: Some(ws_url),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let on_interrupted = Arc::new(|| {});

        let mut client = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted)
            .await
            .unwrap();

        let setup_val = setup_rx.await.unwrap();
        let tools = setup_val["setup"]["tools"]
            .as_array()
            .expect("tools must be present");
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "stop_dialogue");

        client.stop().await;
    }

    #[tokio::test]
    async fn test_bluetooth_tool_declarations_sent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{}/ws", port);

        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<Value>();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    if let Some(Ok(Message::Text(setup_text))) = ws.next().await {
                        let val: Value = serde_json::from_str(&setup_text).unwrap();
                        let _ = setup_tx.send(val);
                    }
                    let resp = json!({ "setupComplete": {} });
                    let _ = ws.send(Message::Text(resp.to_string())).await;
                    while ws.next().await.is_some() {}
                }
            }
        });

        let cfg = LiveClientConfig {
            api_key: "dummy".to_string(),
            model: "models/gemini-2.0-flash-exp".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: true,
            has_stop_dialogue: false,
            endpoint: Some(ws_url),
            cancellation_epoch: None,
            authorization: None,
            suppress_followup: None,
        };

        let (_tx_user, rx_user) = mpsc::channel(10);
        let (tx_model, _rx_model) = mpsc::channel(10);
        let on_interrupted = Arc::new(|| {});

        let mut client = GeminiLiveClient::connect(cfg, rx_user, tx_model, None, on_interrupted)
            .await
            .unwrap();

        let setup_val = setup_rx.await.unwrap();
        let tools = setup_val["setup"]["tools"]
            .as_array()
            .expect("tools must be present");
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "enable_bluetooth");

        client.stop().await;
    }

    fn model_pcm(value: u8) -> Value {
        json!({"serverContent":{"modelTurn":{"parts":[{"inlineData":{
            "mimeType":"audio/pcm;rate=24000","data":BASE64.encode([value, 0])
        }}]}}})
    }

    fn model_tool() -> Value {
        json!({"toolCall":{"functionCalls":[{"id":"fixture-call","name":"get_weather","args":{"location_id":"fixture-city"}}]}})
    }

    fn authorize_input(auth: &TurnAuthorization, turns: &SessionTurns) -> u64 {
        let id = auth.begin_explicit_turn();
        assert!(auth.mark_submitted(id));
        assert!(auth.complete_input(id));
        turns.submit(id).unwrap();
        id
    }

    struct ReceiverFixture {
        server: mpsc::UnboundedSender<Message>,
        output: mpsc::Receiver<(u64, u64, Vec<u8>)>,
        tools: mpsc::Receiver<ToolCallRequest>,
        outgoing: mpsc::Receiver<(u64, String)>,
        shutdown: broadcast::Sender<()>,
        task: tokio::task::JoinHandle<Result<()>>,
    }

    impl ReceiverFixture {
        fn new(
            auth: Option<Arc<TurnAuthorization>>,
            turns: Arc<SessionTurns>,
            epoch: Arc<AtomicU64>,
        ) -> Self {
            let (server, incoming) = mpsc::unbounded_channel::<Message>();
            let stream = Box::pin(futures_util::stream::unfold(
                incoming,
                |mut rx| async move { rx.recv().await.map(|frame| (Ok(frame), rx)) },
            ));
            let (tx_output, output) = mpsc::channel(32);
            let (tx_tools, tools) = mpsc::channel(16);
            let (tx_outgoing, outgoing) = mpsc::channel(16);
            let (shutdown, _) = broadcast::channel(2);
            let owner_epoch = epoch.clone();
            let task = tokio::spawn(run_receiver_task(
                stream,
                tx_output,
                Some(tx_tools),
                tx_outgoing,
                Arc::new(move || {
                    owner_epoch.fetch_add(1, Ordering::SeqCst);
                }),
                epoch,
                Arc::new(AtomicU64::new(1)),
                auth,
                turns,
                None,
                shutdown.subscribe(),
            ));
            Self {
                server,
                output,
                tools,
                outgoing,
                shutdown,
                task,
            }
        }

        fn send(&self, value: Value) {
            self.server.send(Message::Text(value.to_string())).unwrap();
        }

        async fn audio(&mut self) -> (u64, u64, Vec<u8>) {
            tokio::time::timeout(std::time::Duration::from_secs(1), self.output.recv())
                .await
                .unwrap()
                .unwrap()
        }

        async fn quiet(&mut self) {
            tokio::select! {
                output = self.output.recv() => panic!("Unauthorized output: metadata {:?}", output.as_ref().map(|(e,r,p)| (*e,*r,p.len()))),
                tool = self.tools.recv() => panic!("Unauthorized tool: {:?}", tool.map(|t| t.function_name)),
                outgoing = self.outgoing.recv() => panic!("Unauthorized tool reply: {:?}", outgoing.map(|(id,_)| id)),
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }
        }

        async fn stop(mut self) {
            let _ = self.shutdown.send(());
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut self.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    impl Drop for ReceiverFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct RecordingSink(mpsc::UnboundedSender<Message>);

    impl Sink<Message> for RecordingSink {
        type Error = tokio_tungstenite::tungstenite::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.0
                .send(item)
                .map_err(|_| tokio_tungstenite::tungstenite::Error::ConnectionClosed)
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    struct SenderFixture {
        input: mpsc::Sender<UserAudio>,
        _tools: mpsc::Sender<(u64, String)>,
        wire: mpsc::UnboundedReceiver<Message>,
        shutdown: broadcast::Sender<()>,
        task: tokio::task::JoinHandle<Result<()>>,
    }

    impl SenderFixture {
        fn new(auth: Option<Arc<TurnAuthorization>>, turns: Arc<SessionTurns>) -> Self {
            let (input, mut rx_input) = mpsc::channel(16);
            let (tools, rx_tools) = mpsc::channel(8);
            let (tx_wire, wire) = mpsc::unbounded_channel();
            let (shutdown, _) = broadcast::channel(2);
            let rx_shutdown = shutdown.subscribe();
            let task = tokio::spawn(async move {
                run_sender_task(
                    RecordingSink(tx_wire),
                    &mut rx_input,
                    rx_tools,
                    auth,
                    turns,
                    rx_shutdown.resubscribe(),
                )
                .await
            });
            Self {
                input,
                _tools: tools,
                wire,
                shutdown,
                task,
            }
        }
        async fn send(&self, turn_id: u64, pcm: Vec<u8>) {
            assert!(self.input.send(UserAudio { turn_id, pcm }).await.is_ok());
        }
        async fn frame(&mut self) -> Value {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), self.wire.recv())
                .await
                .unwrap()
                .unwrap();
            match frame {
                Message::Text(s) => serde_json::from_str(&s).unwrap(),
                _ => panic!("Expected JSON wire message"),
            }
        }
        async fn quiet(&mut self) {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), self.wire.recv())
                    .await
                    .is_err()
            );
        }
        async fn stop(mut self) {
            let _ = self.shutdown.send(());
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut self.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }
    impl Drop for SenderFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn startup_and_missing_authorization_drop_unsolicited_pcm_and_tools() {
        for authorization_present in [false, true] {
            let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
            let turns = Arc::new(SessionTurns::default());
            let mut fixture = ReceiverFixture::new(
                authorization_present.then_some(auth),
                turns,
                Arc::new(AtomicU64::new(0)),
            );
            fixture.send(model_pcm(10));
            fixture.send(model_tool());
            fixture.send(json!({"serverContent":{"generationComplete":true}}));
            fixture.send(json!({"serverContent":{"turnComplete":true}}));
            fixture.quiet().await;
            fixture.stop().await;
        }
    }

    #[tokio::test]
    async fn sender_requires_real_pcm_then_one_end_and_rejects_post_end_input() {
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let turns = Arc::new(SessionTurns::default());
        let mut fixture = SenderFixture::new(Some(auth.clone()), turns.clone());
        let id = auth.begin_explicit_turn();
        fixture.send(id, Vec::new()).await;
        fixture.send(id + 1, vec![1, 0]).await;
        fixture.quiet().await;
        assert_eq!(turns.front(), None);
        assert_eq!(auth.accepting_response(), None);
        fixture.send(id, vec![1, 0]).await;
        let start = fixture.frame().await;
        let audio = fixture.frame().await;
        assert_eq!(start, json!({"realtimeInput":{"activityStart":{}}}));
        assert_eq!(
            audio,
            json!({"realtimeInput":{"audio":{"mimeType":"audio/pcm;rate=16000","data":"AQA="}}})
        );
        assert_eq!(
            auth.accepting_response(),
            None,
            "Partial speech cannot grant output"
        );
        fixture.send(id, Vec::new()).await;
        assert_eq!(
            fixture.frame().await,
            json!({"realtimeInput":{"activityEnd":{}}})
        );
        assert_eq!(turns.front(), Some(id));
        assert_eq!(auth.accepting_response(), Some(id));
        fixture.send(id, vec![2, 0]).await;
        fixture.send(id, Vec::new()).await;
        fixture.quiet().await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn sender_without_authorization_never_transmits_even_pcm() {
        let mut fixture = SenderFixture::new(None, Arc::new(SessionTurns::default()));
        fixture.send(1, vec![1, 0]).await;
        fixture.send(1, Vec::new()).await;
        fixture.quiet().await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn generation_complete_keeps_audio_open_until_turn_complete_and_blocks_repeated_reply() {
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let turns = Arc::new(SessionTurns::default());
        let id = authorize_input(&auth, &turns);
        let mut fixture =
            ReceiverFixture::new(Some(auth.clone()), turns, Arc::new(AtomicU64::new(0)));
        fixture.send(model_pcm(1));
        let first = fixture.audio().await;
        fixture.send(json!({"serverContent":{"generationComplete":true}}));
        fixture.quiet().await;
        assert_eq!(auth.accepting_response(), Some(id));
        fixture.send(model_pcm(2));
        let second = fixture.audio().await;
        assert_eq!(second.1, first.1);
        assert_eq!(second.2, vec![2, 0]);
        fixture.send(json!({"serverContent":{"turnComplete":true}}));
        let end = fixture.audio().await;
        assert_eq!((end.0, end.1), (first.0, first.1));
        assert!(end.2.is_empty());
        assert_eq!(auth.accepting_response(), None);
        assert!(auth.is_current(id), "Queued audio retains drain authority");
        fixture.send(model_pcm(3));
        fixture.send(model_tool());
        fixture.quiet().await;
        auth.finish(id);
        assert!(!auth.is_current(id));
        fixture.stop().await;
    }

    #[tokio::test]
    async fn cancelled_response_cannot_retag_late_pcm_or_tools_as_the_next_permit() {
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let turns = Arc::new(SessionTurns::default());
        let old = authorize_input(&auth, &turns);
        let epoch = Arc::new(AtomicU64::new(0));
        let mut fixture = ReceiverFixture::new(Some(auth.clone()), turns.clone(), epoch.clone());
        fixture.send(model_pcm(1));
        assert_eq!(fixture.audio().await.0, 0);
        auth.cancel();
        epoch.fetch_add(1, Ordering::SeqCst);
        let next = authorize_input(&auth, &turns);
        assert_ne!(old, next);
        fixture.send(model_pcm(2));
        fixture.send(model_tool());
        fixture.quiet().await;
        fixture.send(json!({"serverContent":{"interrupted":true}}));
        fixture.send(json!({"serverContent":{"turnComplete":true}}));
        fixture.send(model_pcm(3));
        let actual = fixture.audio().await;
        assert_eq!(actual.0, 2);
        assert_eq!(actual.2, vec![3, 0]);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn cancellation_before_first_model_pcm_preserves_old_wire_turn_identity() {
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let turns = Arc::new(SessionTurns::default());
        let old = authorize_input(&auth, &turns);
        auth.cancel();
        let next = authorize_input(&auth, &turns);
        let mut fixture = ReceiverFixture::new(
            Some(auth.clone()),
            turns.clone(),
            Arc::new(AtomicU64::new(1)),
        );
        fixture.send(model_pcm(1));
        fixture.send(model_tool());
        fixture.send(json!({"serverContent":{"turnComplete":true}}));
        fixture.quiet().await;
        assert_eq!(turns.front(), Some(next));
        assert_ne!(old, next);
        assert_eq!(auth.accepting_response(), Some(next));
        fixture.send(model_pcm(2));
        assert_eq!(fixture.audio().await.2, vec![2, 0]);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn cancelled_open_activity_closes_once_and_stale_input_cannot_reopen_it() {
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let turns = Arc::new(SessionTurns::default());
        let mut fixture = SenderFixture::new(Some(auth.clone()), turns.clone());
        let old = auth.begin_explicit_turn();
        fixture.send(old, vec![1, 0]).await;
        fixture.frame().await;
        fixture.frame().await;
        auth.cancel();
        let next = auth.begin_explicit_turn();
        fixture.send(old, vec![2, 0]).await;
        fixture.send(old, Vec::new()).await;
        fixture.send(next, vec![3, 0]).await;
        assert_eq!(
            fixture.frame().await,
            json!({"realtimeInput":{"activityEnd":{}}})
        );
        assert_eq!(
            fixture.frame().await,
            json!({"realtimeInput":{"activityStart":{}}})
        );
        assert_eq!(
            fixture.frame().await["realtimeInput"]["audio"]["data"],
            "AwA="
        );
        fixture.send(next, Vec::new()).await;
        assert_eq!(
            fixture.frame().await,
            json!({"realtimeInput":{"activityEnd":{}}})
        );
        assert_eq!(turns.front(), Some(old));
        assert_eq!(auth.accepting_response(), Some(next));
        fixture.send(old, vec![4, 0]).await;
        fixture.send(next, vec![5, 0]).await;
        fixture.quiet().await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn authorized_tool_is_tagged_and_cancelled_result_cannot_be_sent() {
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let turns = Arc::new(SessionTurns::default());
        let id = authorize_input(&auth, &turns);
        let mut fixture =
            ReceiverFixture::new(Some(auth.clone()), turns, Arc::new(AtomicU64::new(0)));
        fixture.send(model_tool());
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), fixture.tools.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.turn_id, id);
        auth.cancel();
        let _ = request.reply_tx.send(json!({"status":"fixture-success"}));
        fixture.quiet().await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn tool_call_lifecycle_does_not_abort_turn_and_delivers_post_tool_audio() {
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let turns = Arc::new(SessionTurns::default());
        let id = authorize_input(&auth, &turns);
        let mut fixture = ReceiverFixture::new(
            Some(auth.clone()),
            turns.clone(),
            Arc::new(AtomicU64::new(0)),
        );

        // Server sends toolCall and turnComplete (the yield)
        fixture.send(model_tool());
        fixture.send(json!({"serverContent":{"turnComplete":true}}));

        // Tool request is received
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), fixture.tools.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.turn_id, id);

        // Crucial assertions: turn must NOT be finished yet!
        assert_eq!(
            turns.front(),
            Some(id),
            "turn must not be acknowledged during tool yield"
        );
        assert_eq!(
            auth.accepting_response(),
            Some(id),
            "auth must still accept response"
        );

        // Tool executes and sends response
        let _ = request
            .reply_tx
            .send(json!({"status":"weather-batumi-22c"}));

        // Outgoing tool response was forwarded to WebSocket
        let (out_id, out_msg) =
            tokio::time::timeout(std::time::Duration::from_secs(1), fixture.outgoing.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(out_id, id);
        assert!(out_msg.contains("weather-batumi-22c"));

        // Server sends post-tool audio and final turnComplete
        fixture.send(model_pcm(99));
        fixture.send(json!({"serverContent":{"turnComplete":true}}));

        // Audio must be received!
        let audio = tokio::time::timeout(std::time::Duration::from_secs(1), fixture.output.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            audio.2,
            vec![99, 0],
            "Model audio after tool response must reach speaker"
        );

        // Empty PCM end-marker must be received
        let end_marker =
            tokio::time::timeout(std::time::Duration::from_secs(1), fixture.output.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(
            end_marker.2.is_empty(),
            "End of response audio marker must be sent"
        );

        // Turn is now completed
        assert_eq!(
            turns.front(),
            None,
            "Turn must be acknowledged after final turnComplete"
        );

        fixture.stop().await;
    }
    #[tokio::test]
    async fn reconnect_revokes_old_permit_and_does_not_replay_queued_input_or_output() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/ws", listener.local_addr().unwrap());
        let (close_first, wait_close_first) = oneshot::channel::<()>();
        let (second_ready, wait_second_ready) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(8), async move {
                let (stream, _) = listener.accept().await?;
                let mut first = tokio_tungstenite::accept_async(stream).await?;
                let _ = first.next().await;
                first
                    .send(Message::Text(json!({"setupComplete":{}}).to_string()))
                    .await?;
                wait_close_first.await?;
                let _ = first.close(None).await;
                drop(first);

                let (stream, _) = listener.accept().await?;
                let mut second = tokio_tungstenite::accept_async(stream).await?;
                let setup = second
                    .next()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("No setup"))??;
                let setup: Value = serde_json::from_slice(setup.into_data().as_slice())?;
                anyhow::ensure!(
                    setup["setup"]["realtimeInputConfig"]["automaticActivityDetection"]["disabled"]
                        == true
                );
                second
                    .send(Message::Text(json!({"setupComplete":{}}).to_string()))
                    .await?;
                second.send(Message::Text(model_pcm(4).to_string())).await?;
                second.send(Message::Text(model_tool().to_string())).await?;
                second
                    .send(Message::Text(
                        json!({"serverContent":{"turnComplete":true}}).to_string(),
                    ))
                    .await?;
                let _ = second_ready.send(());

                let mut activity_start_count = 0;
                let mut audio_count = 0;
                while let Some(frame) = second.next().await {
                    let Message::Text(text) = frame? else {
                        continue;
                    };
                    let message: Value = serde_json::from_str(&text)?;
                    anyhow::ensure!(
                        message.get("clientContent").is_none(),
                        "No generation-triggering clientContent expected"
                    );
                    if message["realtimeInput"].get("activityStart").is_some() {
                        activity_start_count += 1;
                    }
                    if let Some(data) = message["realtimeInput"]["audio"]["data"].as_str() {
                        anyhow::ensure!(data == "CQA=", "Stale input replayed after reconnect");
                        audio_count += 1;
                    }
                    if message["realtimeInput"].get("activityEnd").is_some() {
                        break;
                    }
                }
                anyhow::ensure!(
                    activity_start_count == 1 && audio_count == 1,
                    "Expected one fresh activity only"
                );
                second.send(Message::Text(model_pcm(9).to_string())).await?;
                second
                    .send(Message::Text(
                        json!({"serverContent":{"turnComplete":true}}).to_string(),
                    ))
                    .await?;
                while let Some(frame) = second.next().await {
                    if frame.is_err() || matches!(frame, Ok(Message::Close(_))) {
                        break;
                    }
                }
                anyhow::Ok(())
            })
            .await
        });
        let auth = Arc::new(TurnAuthorization::new(std::time::Duration::from_secs(5)));
        let cfg = LiveClientConfig {
            api_key: "UNUSED_FIXTURE".into(),
            model: "models/fixture".into(),
            voice_name: "Aoede".into(),
            system_instruction: "fixture".into(),
            has_ha_tools: false,
            has_weather_tools: true,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(endpoint),
            cancellation_epoch: Some(Arc::new(AtomicU64::new(0))),
            authorization: Some(auth.clone()),
            suppress_followup: None,
        };
        let (tx_input, rx_input) = mpsc::channel(16);
        let (tx_output, mut rx_output) = mpsc::channel(16);
        let (tx_tools, mut rx_tools) = mpsc::channel(8);
        let mut client =
            GeminiLiveClient::connect(cfg, rx_input, tx_output, Some(tx_tools), Arc::new(|| {}))
                .await
                .unwrap();
        let old = auth.begin_explicit_turn();
        let _ = close_first.send(());
        assert!(tx_input
            .send(UserAudio {
                turn_id: old,
                pcm: vec![7, 0]
            })
            .await
            .is_ok());
        assert!(tx_input
            .send(UserAudio {
                turn_id: old,
                pcm: Vec::new()
            })
            .await
            .is_ok());
        tokio::time::timeout(std::time::Duration::from_secs(3), wait_second_ready)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !client.is_ready() {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(auth.current_turn(), None);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), rx_output.recv())
                .await
                .is_err()
        );
        assert!(rx_tools.try_recv().is_err());
        let fresh = auth.begin_explicit_turn();
        assert_ne!(fresh, old);
        assert!(tx_input
            .send(UserAudio {
                turn_id: fresh,
                pcm: vec![9, 0]
            })
            .await
            .is_ok());
        assert!(tx_input
            .send(UserAudio {
                turn_id: fresh,
                pcm: Vec::new()
            })
            .await
            .is_ok());
        let actual = tokio::time::timeout(std::time::Duration::from_secs(1), rx_output.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(actual.2, vec![9, 0]);
        let end = tokio::time::timeout(std::time::Duration::from_secs(1), rx_output.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(end.2.is_empty());
        client.stop().await;
        assert_eq!(auth.current_turn(), None);
        server.await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn alarm_survives_remote_interrupt_and_two_turns_finish_on_one_socket() {
        use crate::audio::{AudioPlayback, PlaybackBackend, ALARM_RESPONSE_ID};
        use std::time::Duration;

        async fn next_json(
            socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        ) -> Result<Value> {
            while let Some(frame) = socket.next().await {
                if let Message::Text(text) = frame? {
                    return Ok(serde_json::from_str(&text)?);
                }
            }
            anyhow::bail!("Missing synthetic client frame");
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/ws", listener.local_addr().unwrap());
        let (send_interrupt, wait_interrupt) = oneshot::channel();
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(6), async move {
                let (stream, _) = listener.accept().await?;
                let mut ws = tokio_tungstenite::accept_async(stream).await?;
                let setup = next_json(&mut ws).await?;
                anyhow::ensure!(setup.get("setup").is_some());
                let complete = json!({"serverContent":{"turnComplete":true}});
                for frame in [
                    json!({"setupComplete":{}}),
                    model_pcm(90),
                    model_tool(),
                    complete.clone(),
                ] {
                    ws.send(Message::Text(frame.to_string())).await?;
                }
                wait_interrupt.await?;
                for frame in [
                    json!({"serverContent":{"interrupted":true}}),
                    model_pcm(91),
                    complete.clone(),
                ] {
                    ws.send(Message::Text(frame.to_string())).await?;
                }
                for sample in [11, 21] {
                    let start = next_json(&mut ws).await?;
                    anyhow::ensure!(start == json!({"realtimeInput":{"activityStart":{}}}));
                    let audio = next_json(&mut ws).await?;
                    anyhow::ensure!(
                        audio
                            == json!({"realtimeInput":{"audio":{
                                "mimeType":"audio/pcm;rate=16000", "data":BASE64.encode([sample, 0])
                            }}}),
                        "Only fresh synthetic PCM may reach this turn"
                    );
                    let end = next_json(&mut ws).await?;
                    anyhow::ensure!(end == json!({"realtimeInput":{"activityEnd":{}}}));
                    for frame in [
                        model_pcm(sample),
                        json!({"serverContent":{"generationComplete":true}}),
                        model_pcm(sample + 1),
                        complete.clone(),
                        model_pcm(92),
                        complete.clone(),
                    ] {
                        ws.send(Message::Text(frame.to_string())).await?;
                    }
                }
                while let Some(Ok(frame)) = ws.next().await {
                    anyhow::ensure!(
                        !matches!(frame, Message::Text(_)),
                        "Unexpected synthetic input after both turns"
                    );
                    if matches!(frame, Message::Close(_)) {
                        break;
                    }
                }
                anyhow::Ok(())
            })
            .await
        });

        let playback = Arc::new(
            AudioPlayback::start_with_backend("fixture".into(), 24_000, PlaybackBackend::Mock)
                .unwrap(),
        );
        let auth = Arc::new(TurnAuthorization::new(Duration::from_secs(5)));
        let interrupted = Arc::new(tokio::sync::Notify::new());
        let callback = {
            let playback = playback.clone();
            let interrupted = interrupted.clone();
            Arc::new(move || {
                playback.interrupt();
                interrupted.notify_one();
            })
        };
        let cfg = LiveClientConfig {
            api_key: "UNUSED_FIXTURE".into(),
            model: "models/fixture".into(),
            voice_name: "Aoede".into(),
            system_instruction: "fixture".into(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_volume_tools: false,
            has_bluetooth_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(endpoint),
            cancellation_epoch: Some(playback.cancellation_epoch_handle()),
            authorization: Some(auth.clone()),
            suppress_followup: None,
        };
        let (tx_input, rx_input) = mpsc::channel(8);
        let (tx_output, mut rx_output) = mpsc::channel(8);
        let (tx_tools, mut rx_tools) = mpsc::channel(8);
        let mut client = tokio::time::timeout(
            Duration::from_secs(2),
            GeminiLiveClient::connect(cfg, rx_input, tx_output, Some(tx_tools), callback),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(25), rx_output.recv())
                .await
                .is_err()
        );
        assert!(rx_tools.try_recv().is_err());
        let alarm_epoch = playback.claim_alarm();
        playback
            .play(alarm_epoch, ALARM_RESPONSE_ID, vec![1, 0])
            .await
            .unwrap();
        send_interrupt.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), interrupted.notified())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), rx_output.recv())
                .await
                .is_err()
        );
        assert_eq!(playback.cancellation_epoch(), alarm_epoch);
        assert!(playback.is_playing());
        assert!(playback.release_alarm(alarm_epoch));

        let mut previous_response = None;
        for sample in [11, 21] {
            let turn_id = auth.begin_explicit_turn();
            for pcm in [vec![sample, 0], Vec::new()] {
                tx_input.send(UserAudio { turn_id, pcm }).await.unwrap();
            }
            let mut this_response = None;
            for expected in [vec![sample, 0], vec![sample + 1, 0], Vec::new()] {
                let (epoch, id, pcm) =
                    tokio::time::timeout(Duration::from_secs(1), rx_output.recv())
                        .await
                        .unwrap()
                        .unwrap();
                assert_eq!(pcm, expected);
                assert_eq!(*this_response.get_or_insert(id), id);
                assert_ne!(Some(id), previous_response);
                playback.play(epoch, id, pcm).await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                while playback.is_playing() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(25), rx_output.recv())
                    .await
                    .is_err()
            );
            auth.finish(turn_id);
            tx_input
                .send(UserAudio {
                    turn_id,
                    pcm: vec![93, 0],
                })
                .await
                .unwrap();
            previous_response = this_response;
        }
        tokio::time::timeout(Duration::from_secs(1), client.stop())
            .await
            .unwrap();
        server.await.unwrap().unwrap().unwrap();
    }
}
