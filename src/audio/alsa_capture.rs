use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// Channel selection mode for the 8-channel PDM microphone array
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelMode {
    /// Use only primary microphone (channel 0)
    #[allow(dead_code)]
    Ch0,
    /// Delay-and-sum beamformer: average of 4 physical mics (channels 0..3)
    /// Attenuates incoherent ambient noise by 6 dB
    #[default]
    Avg4,
    /// Custom channel index (0..7)
    #[allow(dead_code)]
    Ch(usize),
}

/// Decimator and downmixer: converts 8-channel S32_LE @ 48kHz to 1-channel S16_LE @ 16kHz (3:1 integer decimation).
/// Uses a 3-tap FIR anti-aliasing filter to prevent aliasing distortion above 8kHz.
pub struct Decimator {
    channel_mode: ChannelMode,
    bit_shift: u32,
    gain: f32,
    raw_buffer: Vec<u8>,
    out_chunk_buffer: Vec<u8>,
}

impl Decimator {
    /// Create new Decimator with specified channel mode, bit shift (default 16 for PDM), and gain.
    pub fn new(channel_mode: ChannelMode, bit_shift: u32, gain: f32) -> Self {
        Self {
            channel_mode,
            bit_shift,
            gain,
            raw_buffer: Vec::with_capacity(4096),
            out_chunk_buffer: Vec::with_capacity(512),
        }
    }

    /// Default configuration for Station Mini 1 (YNDX-0004) PDM array:
    /// 4-mic beamforming average, 16-bit right shift from S32_LE, 1.0x gain.
    #[allow(dead_code)]
    pub fn default_mini() -> Self {
        Self::new(ChannelMode::Avg4, 16, 1.0)
    }

    /// Extract single sample from 8-channel S32_LE frame (32 bytes = 8 x 4 bytes)
    #[inline]
    fn extract_frame_sample(&self, frame_bytes: &[u8]) -> i32 {
        let (ch_pairs, _) = frame_bytes.as_chunks::<4>();
        match self.channel_mode {
            ChannelMode::Ch0 => {
                if !ch_pairs.is_empty() {
                    i32::from_le_bytes(ch_pairs[0])
                } else {
                    0
                }
            }
            ChannelMode::Avg4 => {
                let count = ch_pairs.len().min(4);
                if count > 0 {
                    let sum: i64 = ch_pairs[..count]
                        .iter()
                        .map(|b| i32::from_le_bytes(*b) as i64)
                        .sum();
                    (sum / count as i64) as i32
                } else {
                    0
                }
            }
            ChannelMode::Ch(idx) => {
                if idx < ch_pairs.len() {
                    i32::from_le_bytes(ch_pairs[idx])
                } else {
                    0
                }
            }
        }
    }

    /// Process incoming raw bytes from ALSA capture stream (8ch S32_LE 48kHz).
    /// Returns 16ms chunks of 1-channel S16_LE 16kHz PCM (512 bytes = 256 samples).
    pub fn process(&mut self, incoming: &[u8]) -> Vec<Vec<u8>> {
        self.raw_buffer.extend_from_slice(incoming);

        let mut ready_chunks = Vec::new();
        // 3 frames @ 48kHz (3 * 32 bytes = 96 bytes) produce 1 output sample @ 16kHz
        const BYTES_PER_48K_FRAME: usize = 32; // 8 channels * 4 bytes
        const BYTES_PER_DECIMATION_BLOCK: usize = BYTES_PER_48K_FRAME * 3; // 96 bytes
        const TARGET_CHUNK_BYTES: usize = 512; // 256 samples * 2 bytes = 16ms @ 16kHz

        let block_count = self.raw_buffer.len() / BYTES_PER_DECIMATION_BLOCK;
        if block_count == 0 {
            return ready_chunks;
        }

        for i in 0..block_count {
            let offset = i * BYTES_PER_DECIMATION_BLOCK;
            let f0 = &self.raw_buffer[offset..offset + BYTES_PER_48K_FRAME];
            let f1 =
                &self.raw_buffer[offset + BYTES_PER_48K_FRAME..offset + 2 * BYTES_PER_48K_FRAME];
            let f2 = &self.raw_buffer
                [offset + 2 * BYTES_PER_48K_FRAME..offset + 3 * BYTES_PER_48K_FRAME];

            let s0 = self.extract_frame_sample(f0) as i64;
            let s1 = self.extract_frame_sample(f1) as i64;
            let s2 = self.extract_frame_sample(f2) as i64;

            // 3-tap FIR anti-aliasing triangular low-pass filter: y = (s0 + 2*s1 + s2) / 4
            let filtered = (s0 + 2 * s1 + s2) / 4;

            // Scale and bit shift from 32-bit PDM to 16-bit PCM
            let shifted = filtered >> self.bit_shift;
            let with_gain = (shifted as f32) * self.gain;
            let clamped = with_gain.clamp(-32768.0, 32767.0) as i16;

            self.out_chunk_buffer
                .extend_from_slice(&clamped.to_le_bytes());

            if self.out_chunk_buffer.len() >= TARGET_CHUNK_BYTES {
                ready_chunks.push(self.out_chunk_buffer.split_off(TARGET_CHUNK_BYTES));
                // Swap so out_chunk_buffer contains remaining, ready_chunks gets 512 bytes
                let last = ready_chunks.len() - 1;
                std::mem::swap(&mut ready_chunks[last], &mut self.out_chunk_buffer);
            }
        }

        let consumed = block_count * BYTES_PER_DECIMATION_BLOCK;
        self.raw_buffer.drain(..consumed);

        ready_chunks
    }
}

/// Configuration for direct ALSA microphone capture
#[derive(Debug, Clone)]
pub struct AlsaCaptureConfig {
    pub device: String,
    pub channel_mode: ChannelMode,
    pub bit_shift: u32,
    pub gain: f32,
    pub custom_arecord_path: Option<PathBuf>,
}

impl Default for AlsaCaptureConfig {
    fn default() -> Self {
        Self {
            device: "hw:0,2".to_string(),
            channel_mode: ChannelMode::Avg4,
            bit_shift: 16,
            gain: 1.0,
            custom_arecord_path: None,
        }
    }
}

/// Direct ALSA capture manager running `arecord` to capture raw PDM mic streams from `hw:0,2`
pub struct AlsaCapture {
    child: Arc<std::sync::Mutex<Option<Child>>>,
    is_running: Arc<AtomicBool>,
}

impl AlsaCapture {
    /// Start capturing audio from hardware ALSA device, downmixing and decimating to 16kHz mono PCM.
    pub fn start(config: AlsaCaptureConfig, tx_mic: mpsc::Sender<Vec<u8>>) -> Result<Self> {
        let is_running = Arc::new(AtomicBool::new(true));
        let is_running_clone = is_running.clone();
        let child_arc = Arc::new(std::sync::Mutex::new(None));
        let child_for_worker = child_arc.clone();

        let arecord_bin = detect_arecord(config.custom_arecord_path.as_deref());

        #[cfg(test)]
        let arecord_bin = {
            if std::env::var("MINI_ARECORD_PATH").is_ok()
                || std::env::var("MINI_CAPTURE_SOURCE_PATH").is_ok()
            {
                arecord_bin
            } else {
                None
            }
        };

        let capture_path = match arecord_bin {
            Some(path) => path,
            None => {
                #[cfg(not(test))]
                anyhow::bail!("No capture executable available; microphone unavailable");
                #[cfg(test)]
                {
                    // Explicitly test-only: production must never announce a silent mock as ready.
                    let is_running_mock = is_running.clone();
                    tokio::spawn(async move {
                        let mut ticker =
                            tokio::time::interval(std::time::Duration::from_millis(16));
                        while is_running_mock.load(Ordering::SeqCst) {
                            ticker.tick().await;
                            if tx_mic.send(vec![0u8; 512]).await.is_err() {
                                break;
                            }
                        }
                    });
                    return Ok(Self {
                        child: child_arc,
                        is_running,
                    });
                }
            }
        };

        let is_mini_pcm_source = capture_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.contains("mini_pcm_source"))
            .unwrap_or(false);

        info!(
            "Starting ALSA microphone capture via {} on device {} (8ch S32_LE 48kHz)...",
            capture_path.display(),
            config.device
        );

        let mut cmd = Command::new(&capture_path);
        if is_mini_pcm_source {
            cmd.args(["-D", &config.device]);
            cmd.env("LD_LIBRARY_PATH", "/system/vendor/quasar/libs");
        } else {
            cmd.args([
                "-D",
                &config.device,
                "-c",
                "8",
                "-r",
                "48000",
                "-f",
                "S32_LE",
                "-t",
                "raw",
                "-q",
            ]);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn capture binary at {}",
                capture_path.display()
            )
        })?;

        let stdout = child
            .stdout
            .take()
            .context("Failed to take capture stdout pipe")?;
        {
            let mut lock = child_for_worker.lock().unwrap();
            *lock = Some(child);
        }

        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = stdout;
            let mut decimator = Decimator::new(config.channel_mode, config.bit_shift, config.gain);
            let mut buf = [0u8; 4096];

            while is_running_clone.load(Ordering::SeqCst) {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        info!("AlsaCapture arecord stdout reached EOF");
                        break;
                    }
                    Ok(n) => {
                        let chunks = decimator.process(&buf[..n]);
                        for chunk in chunks {
                            if tx_mic.blocking_send(chunk).is_err() {
                                is_running_clone.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        error!("AlsaCapture read error: {}", e);
                        break;
                    }
                }
            }
            is_running_clone.store(false, Ordering::SeqCst);
        });

        Ok(Self {
            child: child_arc,
            is_running,
        })
    }

    /// Check if capture is running
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }

    /// Stop capture child process and reap it cleanly
    pub fn stop(&mut self) {
        self.is_running.store(false, Ordering::SeqCst);
        if let Ok(mut lock) = self.child.lock() {
            if let Some(mut child) = lock.take() {
                debug!("Stopping AlsaCapture child process...");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl Drop for AlsaCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn detect_arecord(custom_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = custom_path {
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }

    if let Ok(p) = std::env::var("MINI_CAPTURE_SOURCE_PATH") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(p) = std::env::var("MINI_ARECORD_PATH") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    let candidates = [
        "/data/opt/mini-assistant/mini_pcm_source",
        "./mini_pcm_source",
        "/system/workdir/bin/arecord",
        "/usr/bin/arecord",
    ];
    for cand in candidates {
        let p = Path::new(cand);
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }

    if let Ok(path) = which_bin("mini_pcm_source") {
        return Some(path);
    }

    if let Ok(path) = which_bin("arecord") {
        return Some(path);
    }

    None
}

fn which_bin(name: &str) -> Result<PathBuf> {
    if let Ok(paths) = std::env::var("PATH") {
        for dir in paths.split(':') {
            let p = Path::new(dir).join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    anyhow::bail!("Binary '{}' not found in PATH", name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decimator_ratio_and_alignment() {
        let mut decimator = Decimator::new(ChannelMode::Ch0, 16, 1.0);
        // 96 bytes = 3 frames of 8-channel S32_LE (3 * 32 = 96)
        let mut block = vec![0u8; 96];
        // In frame 0, set channel 0 to 1000 << 16
        let val: i32 = 1000 << 16;
        block[0..4].copy_from_slice(&val.to_le_bytes());
        // In frame 1, set channel 0 to 2000 << 16
        let val1: i32 = 2000 << 16;
        block[32..36].copy_from_slice(&val1.to_le_bytes());
        // In frame 2, set channel 0 to 1000 << 16
        let val2: i32 = 1000 << 16;
        block[64..68].copy_from_slice(&val2.to_le_bytes());

        // Process should consume the 96 bytes and accumulate 1 sample (2 bytes)
        let chunks = decimator.process(&block);
        assert!(
            chunks.is_empty(),
            "Chunk requires 512 bytes before emitting"
        );
        assert_eq!(decimator.out_chunk_buffer.len(), 2);

        // Filter calculation: (1000 + 2*2000 + 1000) / 4 = 6000 / 4 = 1500
        let s = i16::from_le_bytes(decimator.out_chunk_buffer[0..2].try_into().unwrap());
        assert_eq!(s, 1500);
    }

    #[test]
    fn test_decimator_avg4_extraction() {
        let mut decimator = Decimator::new(ChannelMode::Avg4, 16, 1.0);
        let mut block = vec![0u8; 96];

        // Frame 0: ch0=100, ch1=200, ch2=300, ch3=400 (avg = 250)
        let ch0: i32 = 100 << 16;
        let ch1: i32 = 200 << 16;
        let ch2: i32 = 300 << 16;
        let ch3: i32 = 400 << 16;
        block[0..4].copy_from_slice(&ch0.to_le_bytes());
        block[4..8].copy_from_slice(&ch1.to_le_bytes());
        block[8..12].copy_from_slice(&ch2.to_le_bytes());
        block[12..16].copy_from_slice(&ch3.to_le_bytes());

        // Frame 1 and 2: same values so filter produces exact 250
        block[32..36].copy_from_slice(&ch0.to_le_bytes());
        block[36..40].copy_from_slice(&ch1.to_le_bytes());
        block[40..44].copy_from_slice(&ch2.to_le_bytes());
        block[44..48].copy_from_slice(&ch3.to_le_bytes());

        block[64..68].copy_from_slice(&ch0.to_le_bytes());
        block[68..72].copy_from_slice(&ch1.to_le_bytes());
        block[72..76].copy_from_slice(&ch2.to_le_bytes());
        block[76..80].copy_from_slice(&ch3.to_le_bytes());

        let _ = decimator.process(&block);
        assert_eq!(decimator.out_chunk_buffer.len(), 2);
        let s = i16::from_le_bytes(decimator.out_chunk_buffer[0..2].try_into().unwrap());
        assert_eq!(s, 250);
    }

    #[test]
    fn test_decimator_chunk_framing() {
        let mut decimator = Decimator::new(ChannelMode::Ch0, 16, 1.0);
        // Feed exactly 256 blocks of 96 bytes = 24,576 bytes
        // 256 output samples = 512 bytes = exactly 1 chunk
        let block = vec![0u8; 96 * 256];
        let chunks = decimator.process(&block);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 512);
        assert_eq!(decimator.out_chunk_buffer.len(), 0);
    }

    #[test]
    fn test_decimator_clamping() {
        let mut decimator = Decimator::new(ChannelMode::Ch0, 0, 1.0);
        let mut block = vec![0u8; 96];
        let loud: i32 = 100_000;
        block[0..4].copy_from_slice(&loud.to_le_bytes());
        block[32..36].copy_from_slice(&loud.to_le_bytes());
        block[64..68].copy_from_slice(&loud.to_le_bytes());

        let _ = decimator.process(&block);
        let s = i16::from_le_bytes(decimator.out_chunk_buffer[0..2].try_into().unwrap());
        assert_eq!(s, 32767);
    }

    #[tokio::test]
    async fn test_alsa_capture_mock_lifecycle() {
        let (tx, mut rx) = mpsc::channel(10);
        let config = AlsaCaptureConfig::default();
        let mut capture = AlsaCapture::start(config, tx).unwrap();
        assert!(capture.is_running());

        // In mock mode without arecord binary, it emits silence chunks
        let first_chunk = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("Must receive chunk")
            .expect("Channel must not be empty");
        assert_eq!(first_chunk.len(), 512);

        capture.stop();
        assert!(!capture.is_running());
    }
}
