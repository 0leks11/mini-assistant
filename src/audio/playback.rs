use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info};

/// Audio Playback Controller using native ALSA aplay process streaming
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
            info!("Starting ALSA audio playback worker on '{}' ({} Hz)...", device_name, sample_rate);

            // Locate aplay binary on system (stock Yandex uses /system/workdir/bin/aplay or /usr/bin/aplay)
            let aplay_path = if std::path::Path::new("/system/workdir/bin/aplay").exists() {
                "/system/workdir/bin/aplay"
            } else if std::path::Path::new("/usr/bin/aplay").exists() {
                "/usr/bin/aplay"
            } else {
                "aplay"
            };

            let mut current_child: Option<Child> = None;

            while let Some(chunk) = audio_rx.blocking_recv() {
                if interrupted_flag.load(Ordering::SeqCst) {
                    if let Some(mut child) = current_child.take() {
                        debug!("Barge-in interrupt: killing active aplay process");
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    continue;
                }

                // If child process is finished or not spawned yet, spawn new aplay instance
                let need_spawn = match &mut current_child {
                    None => true,
                    Some(child) => match child.try_wait() {
                        Ok(Some(_)) | Err(_) => true,
                        Ok(None) => false,
                    },
                };

                if need_spawn {
                    let child = Command::new(aplay_path)
                        .args([
                            "-q",
                            "-D", &device_name,
                            "-r", &sample_rate.to_string(),
                            "-f", "S16_LE",
                            "-c", "1",
                            "-t", "raw",
                        ])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();

                    match child {
                        Ok(c) => current_child = Some(c),
                        Err(e) => {
                            // On host (macOS dev), aplay might not exist
                            debug!("Mock playback (aplay not found / failed: {}), received {} bytes", e, chunk.len());
                            continue;
                        }
                    }
                }

                if let Some(ref mut child) = current_child {
                    if let Some(ref mut stdin) = child.stdin {
                        if let Err(e) = stdin.write_all(&chunk) {
                            debug!("Failed to write chunk to aplay: {}", e);
                            current_child = None;
                        }
                    }
                }
            }

            if let Some(mut child) = current_child {
                let _ = child.kill();
                let _ = child.wait();
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
