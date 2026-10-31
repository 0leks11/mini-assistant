use anyhow::{Context, Result};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

mod audio;
mod config;
mod gemini;
mod ha;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Load environment variables (.env)
    dotenvy::dotenv().ok();

    // 2. Initialize tracing logger
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("mini_assistant=debug".parse()?),
        )
        .init();

    info!("=== mini-assistant v0.1.0 starting on Yandex Station Mini ===");

    // 3. Load config
    let config = config::Config::load().context("Failed to load configuration")?;
    info!("Assistant Name: {}", config.assistant.name);
    info!("Gemini Voice:   {}", config.assistant.voice);
    info!("Audio Playback: {}", config.audio.playback_device);

    let api_key = match &config.gemini.api_key {
        Some(k) if !k.trim().is_empty() => k.clone(),
        _ => {
            warn!("GEMINI_API_KEY is not set in .env or config.toml!");
            warn!("Please set GEMINI_API_KEY in .env to activate Gemini Live voice stream.");
            info!("Waiting for signal (Ctrl-C to stop)...");
            tokio::signal::ctrl_c().await?;
            return Ok(());
        }
    };

    // 4. Initialize Home Assistant client if configured
    let (ha_client, ha_tools_enabled, device_summary) = match &config.ha {
        Some(ha_cfg) if ha_cfg.token.is_some() => {
            let token = ha_cfg.token.as_ref().unwrap();
            match ha::HomeAssistantClient::new(ha_cfg.url.clone(), token) {
                Ok(client) => {
                    info!("Connecting to Home Assistant at {}...", ha_cfg.url);
                    let summary = match client.get_device_summary().await {
                        Ok(s) => {
                            info!("Synchronized devices from Home Assistant:\n{}", s);
                            s
                        }
                        Err(e) => {
                            warn!("Failed to fetch devices from Home Assistant: {}", e);
                            "Устройства умного дома временно недоступны.".to_string()
                        }
                    };
                    (Some(Arc::new(client)), true, summary)
                }
                Err(e) => {
                    warn!("Invalid Home Assistant configuration: {}", e);
                    (None, false, String::new())
                }
            }
        }
        _ => {
            info!("Home Assistant not configured or token missing. Running voice-only mode.");
            (None, false, String::new())
        }
    };

    // 5. Build dynamic system instruction with HA device list
    let mut system_instruction = format!(
        "Ты — персональный голосовой ассистент станции Яндекс Мини (имя: {}). \
        Отвечай кратко, емко, живо и разговорно по-русски. \
        Не используй markdown-разметку или списки с буллетами в голосовых ответах — говори естественно.",
        config.assistant.name
    );

    if ha_tools_enabled {
        system_instruction.push_str("\n\nТы подключен к умному дому (Home Assistant). \
        Для управления светом, кондиционером Midea, выключателями и устройствами ВСЕГДА вызывай функцию 'control_home_device'. \
        Для ответов на вопросы вроде 'сколько раз включался чайник' или истории устройств вызывай функцию 'get_device_history'. \
        Вот список доступных устройств в доме:\n");
        system_instruction.push_str(&device_summary);
    }

    // 6. Setup Tool Calls handler for Home Assistant
    let (tool_call_tx, mut tool_call_rx) = mpsc::channel::<gemini::ToolCallRequest>(20);

    if let Some(ha_arc) = ha_client {
        tokio::spawn(async move {
            while let Some(req) = tool_call_rx.recv().await {
                let ha = ha_arc.clone();
                let output = match req.function_name.as_str() {
                    "control_home_device" => {
                        let domain = req.args.get("domain").and_then(|v| v.as_str()).unwrap_or("");
                        let service = req.args.get("service").and_then(|v| v.as_str()).unwrap_or("");
                        let entity_id = req.args.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
                        let mut params = req.args.get("parameters").cloned().unwrap_or_else(|| json!({}));

                        if let Some(map) = params.as_object_mut() {
                            map.insert("entity_id".to_string(), json!(entity_id));
                        }

                        match ha.call_service(domain, service, params).await {
                            Ok(res) => json!({ "status": "success", "result": res }),
                            Err(e) => json!({ "status": "error", "message": e.to_string() }),
                        }
                    }
                    "get_device_history" => {
                        let entity_id = req.args.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
                        let hours = req.args.get("hours_ago").and_then(|v| v.as_u64()).unwrap_or(24) as u32;

                        match ha.get_entity_history(entity_id, hours).await {
                            Ok(history) => json!({ "status": "success", "history": history }),
                            Err(e) => json!({ "status": "error", "message": e.to_string() }),
                        }
                    }
                    other => {
                        json!({ "status": "unknown_function", "name": other })
                    }
                };

                let _ = req.reply_tx.send(output);
            }
        });
    }

    // 7. Initialize ALSA Playback (24000 Hz, 16-bit mono from Gemini Live)
    let playback = Arc::new(
        audio::AudioPlayback::start(config.audio.playback_device.clone(), 24000)
            .context("Failed to initialize ALSA audio playback")?,
    );

    let playback_for_interrupt = playback.clone();
    let on_interrupted = Arc::new(move || {
        playback_for_interrupt.interrupt();
    });

    // 8. Channels for audio routing:
    // User mic PCM (16kHz) -> Gemini Live
    let (tx_mic, rx_mic) = mpsc::channel::<Vec<u8>>(100);
    // Gemini Live PCM (24kHz) -> ALSA Playback
    let (tx_speaker, mut rx_speaker) = mpsc::channel::<Vec<u8>>(100);

    // 9. Connect to Gemini Live API
    info!("Connecting to Gemini Live API...");
    let _gemini = gemini::GeminiLiveClient::connect(
        &api_key,
        &config.gemini.model,
        &config.assistant.voice,
        &system_instruction,
        ha_tools_enabled,
        rx_mic,
        tx_speaker,
        if ha_tools_enabled { Some(tool_call_tx) } else { None },
        on_interrupted,
    )
    .await
    .context("Failed to start Gemini Live client")?;

    // 10. Start VQE Audio Capture (Mini yio_audio IPC at 127.0.0.1:9850)
    info!("Starting VQE audio capture from yio_audio:9850...");
    let mut vqe_capture = match audio::VqeCapture::start(tx_mic).await {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("Could not connect to yio_audio:9850: {}", e);
            warn!("(If running on host/Mac, this is expected. On Mini, maind provides yio_audio)");
            None
        }
    };

    // 11. Pipe incoming Gemini audio chunks directly to ALSA playback
    let playback_sink = playback.clone();
    tokio::spawn(async move {
        while let Some(pcm_chunk) = rx_speaker.recv().await {
            if let Err(e) = playback_sink.play(pcm_chunk).await {
                error!("Playback write error: {}", e);
            }
        }
    });

    info!("🚀 mini-assistant full audio + Home Assistant pipeline is active!");
    info!("Press Ctrl+C to terminate cleanly.");

    tokio::signal::ctrl_c().await?;
    info!("Shutting down mini-assistant...");

    if let Some(mut cap) = vqe_capture {
        cap.stop();
    }

    info!("Bye!");
    Ok(())
}
