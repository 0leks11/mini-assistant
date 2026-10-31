use anyhow::{Context, Result};
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

    // 4. Initialize ALSA Playback (24000 Hz, 16-bit mono from Gemini Live)
    let playback = Arc::new(
        audio::AudioPlayback::start(config.audio.playback_device.clone(), 24000)
            .context("Failed to initialize ALSA audio playback")?,
    );

    let playback_for_interrupt = playback.clone();
    let on_interrupted = Arc::new(move || {
        playback_for_interrupt.interrupt();
    });

    // 5. Channels for audio routing:
    // User mic PCM (16kHz) -> Gemini Live
    let (tx_mic, rx_mic) = mpsc::channel::<Vec<u8>>(100);
    // Gemini Live PCM (24kHz) -> ALSA Playback
    let (tx_speaker, mut rx_speaker) = mpsc::channel::<Vec<u8>>(100);

    // 6. Connect to Gemini Live API
    info!("Connecting to Gemini Live API...");
    let _gemini = gemini::GeminiLiveClient::connect(
        &api_key,
        &config.gemini.model,
        &config.assistant.voice,
        rx_mic,
        tx_speaker,
        on_interrupted,
    )
    .await
    .context("Failed to start Gemini Live client")?;

    // 7. Start VQE Audio Capture (Mini yio_audio IPC at 127.0.0.1:9850)
    info!("Starting VQE audio capture from yio_audio:9850...");
    let mut vqe_capture = match audio::VqeCapture::start(tx_mic).await {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("Could not connect to yio_audio:9850: {}", e);
            warn!("(If running on host/Mac, this is expected. On Mini, maind provides yio_audio)");
            None
        }
    };

    // 8. Pipe incoming Gemini audio chunks directly to ALSA playback
    let playback_sink = playback.clone();
    tokio::spawn(async move {
        while let Some(pcm_chunk) = rx_speaker.recv().await {
            if let Err(e) = playback_sink.play(pcm_chunk).await {
                error!("Playback write error: {}", e);
            }
        }
    });

    info!("🚀 mini-assistant full audio pipeline is active!");
    info!("Press Ctrl+C to terminate cleanly.");

    tokio::signal::ctrl_c().await?;
    info!("Shutting down mini-assistant...");

    if let Some(mut cap) = vqe_capture {
        cap.stop();
    }

    info!("Bye!");
    Ok(())
}
