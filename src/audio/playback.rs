use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info};

/// ALSA Playback Controller
pub struct AudioPlayback {
    audio_tx: mpsc::Sender<Vec<u8>>,
    is_interrupted: Arc<AtomicBool>,
}

impl AudioPlayback {
    /// Start the playback worker on the specified ALSA device (e.g. "default" or "hw:0,0").
    /// Takes raw 24kHz 16-bit mono PCM (from Gemini Live).
    pub fn start(device_name: String, sample_rate: u32) -> Result<Self> {
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(100);
        let is_interrupted = Arc::new(AtomicBool::new(false));
        let interrupted_flag = is_interrupted.clone();

        std::thread::spawn(move || {
            info!("Starting ALSA playback worker on '{}' ({} Hz)...", device_name, sample_rate);

            #[cfg(target_os = "linux")]
            {
                use alsa::pcm::{Access, Format, HwParams, PCM};
                use alsa::Direction;

                let pcm = match PCM::new(&device_name, Direction::Playback, false) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to open ALSA playback device '{}': {}", device_name, e);
                        return;
                    }
                };

                let hwp = match HwParams::any(&pcm) {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Failed to get HW params: {}", e);
                        return;
                    }
                };

                if let Err(e) = hwp.set_channels(1) {
                    error!("Failed to set mono channel: {}", e);
                    return;
                }
                if let Err(e) = hwp.set_rate(sample_rate, alsa::ValueOr::Nearest) {
                    error!("Failed to set rate {}: {}", sample_rate, e);
                    return;
                }
                if let Err(e) = hwp.set_format(Format::s16()) {
                    error!("Failed to set format S16_LE: {}", e);
                    return;
                }
                if let Err(e) = hwp.set_access(Access::RWInterleaved) {
                    error!("Failed to set RWInterleaved: {}", e);
                    return;
                }
                if let Err(e) = pcm.hw_params(&hwp) {
                    error!("Failed to apply hw_params: {}", e);
                    return;
                }

                let io = pcm.io_bytes();

                while let Some(chunk) = audio_rx.blocking_recv() {
                    if interrupted_flag.load(Ordering::SeqCst) {
                        debug!("Playback interrupted flag set, skipping chunk");
                        let _ = pcm.drop();
                        continue;
                    }

                    if let Err(e) = io.writei(&chunk) {
                        debug!("ALSA write underrun / error: {}, attempting recover", e);
                        let _ = pcm.try_recover(e, false);
                    }
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                // Host dev stub (macOS / Windows mock)
                while let Some(chunk) = audio_rx.blocking_recv() {
                    if interrupted_flag.load(Ordering::SeqCst) {
                        continue;
                    }
                    debug!("Mock audio playback: {} bytes", chunk.len());
                }
            }
        });

        Ok(Self {
            audio_tx,
            is_interrupted,
        })
    }

    /// Queue audio chunk for playback
    pub async fn play(&self, pcm_chunk: Vec<u8>) -> Result<()> {
        self.is_interrupted.store(false, Ordering::SeqCst);
        self.audio_tx.send(pcm_chunk).await.context("Playback channel closed")
    }

    /// Immediately interrupt playback (barge-in when user speaks)
    pub fn interrupt(&self) {
        self.is_interrupted.store(true, Ordering::SeqCst);
    }
}
