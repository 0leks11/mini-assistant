use anyhow::Result;
use tracing::info;

mod audio;
mod config;
mod gemini;
mod ha;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file
    dotenvy::dotenv().ok();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("mini_assistant=debug".parse()?)
        )
        .init();

    info!("mini-assistant starting up...");

    let config = config::Config::load()?;
    info!("Config loaded: assistant name = '{}'", config.assistant.name);

    // TODO Phase 1: audio capture test
    // TODO Phase 2: Gemini Live connection
    // TODO Phase 3: wake word
    // TODO Phase 5: Home Assistant

    info!("mini-assistant initialized (stub). Full pipeline coming soon.");

    // Keep alive
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
