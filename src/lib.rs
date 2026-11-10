pub mod alarms;
pub mod audio;
pub mod bluetooth;
pub mod buttons;
pub mod config;
pub mod gemini;
pub mod ha;
pub mod interaction;
pub mod openai;
pub mod spotify;
pub mod voice;
pub mod wakeword;
pub mod weather;
pub mod web;
pub mod wifi;

pub async fn wait_for_shutdown() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
