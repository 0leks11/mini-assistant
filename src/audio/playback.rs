use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info};

#[allow(dead_code)]
pub const RECORD_MAX: usize = 8192;

/// Playback backend strategy: Station Mini custom `mini_pcm_sink` (preferred on device),
/// standard `aplay` (on host Linux), or `Mock` (for local development/tests without ALSA).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackBackend {
    MiniPcmSink {
        path: PathBuf,
    },
    Aplay {
        path: PathBuf,
        device: String,
        sample_rate: u32,
    },
    Mock,
}

impl PlaybackBackend {
    pub fn detect(device_name: &str, sample_rate: u32) -> Self {
        #[cfg(test)]
        {
            if std::env::var("MINI_PCM_SINK_PATH").is_err() {
                return Self::Mock;
            }
        }

        if let Ok(custom_path) = std::env::var("MINI_PCM_SINK_PATH") {
            let p = PathBuf::from(custom_path);
            if p.exists() {
                return Self::MiniPcmSink { path: p };
            }
        }

        let sink_candidates = ["/data/opt/mini-assistant/mini_pcm_sink"];
        for candidate in sink_candidates {
            let p = Path::new(candidate);
            if p.exists() {
                return Self::MiniPcmSink {
                    path: p.to_path_buf(),
                };
            }
        }

        let aplay_candidates = ["/system/workdir/bin/aplay", "/usr/bin/aplay"];
        for candidate in aplay_candidates {
            let p = Path::new(candidate);
            if p.exists() {
                return Self::Aplay {
                    path: p.to_path_buf(),
                    device: device_name.to_string(),
                    sample_rate,
                };
            }
        }

        if let Ok(path) = which_bin("aplay") {
            return Self::Aplay {
                path,
                device: device_name.to_string(),
                sample_rate,
            };
        }

        Self::Mock
    }
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

/// 16-byte MPCM Preamble: "MPCM" (4), ver=1 (4), rate=48000 (4), ch=2 (2), bits=16 (2)
pub fn make_mpcm_preamble() -> [u8; 16] {
    let mut preamble = [0u8; 16];
    preamble[0..4].copy_from_slice(b"MPCM");
    preamble[4..8].copy_from_slice(&1u32.to_le_bytes());
    preamble[8..12].copy_from_slice(&48000u32.to_le_bytes());
    preamble[12..14].copy_from_slice(&2u16.to_le_bytes());
    preamble[14..16].copy_from_slice(&16u16.to_le_bytes());
    preamble
}

/// 8-byte MPCM Record Header: type (4 bytes LE), length (4 bytes LE)
pub fn make_mpcm_record_header(rec_type: u32, length: u32) -> [u8; 8] {
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(&rec_type.to_le_bytes());
    hdr[4..8].copy_from_slice(&length.to_le_bytes());
    hdr
}

/// Convert 24kHz mono S16_LE PCM to 48kHz stereo S16_LE PCM with:
/// 1. Clamping to [-16000, 16000] to protect `mini_pcm_sink`'s peak guard (17000).
/// 2. Linear interpolation between samples for smooth 2x upsampling.
/// 3. Stereo frame replication (left = right).
pub fn convert_24k_mono_to_48k_stereo(mono_24k: &[u8], last_sample: &mut Option<i16>) -> Vec<u8> {
    let sample_count = mono_24k.len() / 2;
    if sample_count == 0 {
        return Vec::new();
    }
    // Each mono sample produces 2 stereo frames = 4 samples = 8 bytes
    let mut out = Vec::with_capacity(sample_count * 8);

    for i in 0..sample_count {
        let raw = i16::from_le_bytes([mono_24k[2 * i], mono_24k[2 * i + 1]]);
        let s_curr = raw.clamp(-16000, 16000);

        let s_mid = match *last_sample {
            Some(s_prev) => ((s_prev as i32 + s_curr as i32) / 2) as i16,
            None => s_curr,
        };
        *last_sample = Some(s_curr);

        // Frame 0: [s_mid, s_mid]
        out.extend_from_slice(&s_mid.to_le_bytes());
        out.extend_from_slice(&s_mid.to_le_bytes());
        // Frame 1: [s_curr, s_curr]
        out.extend_from_slice(&s_curr.to_le_bytes());
        out.extend_from_slice(&s_curr.to_le_bytes());
    }

    out
}

/// Serializes admission, cancellation and completion of output state.
/// Shared epoch handles are observational: cancellation must use `interrupt()`.
#[derive(Default)]
struct PlaybackLifecycle {
    latest_response: Option<(u64, u64)>,
    ended_response: Option<(u64, u64)>,
    alarm_epoch: Option<u64>,
}

/// Reserved renderer source for a locally scheduled alarm.
pub const ALARM_RESPONSE_ID: u64 = 0xFFFF_FFFF_FFFF_0001;

fn response_has_ended(state: &PlaybackLifecycle, epoch: u64, response_id: u64) -> bool {
    matches!(state.ended_response, Some((ended_epoch, ended_id))
        if ended_epoch == epoch && response_id <= ended_id)
}

fn finish_playback_response(
    lifecycle: &Mutex<PlaybackLifecycle>,
    epoch_counter: &AtomicU64,
    active_response: &AtomicU64,
    playing: &AtomicBool,
    active: &AtomicBool,
    epoch: u64,
    response_id: u64,
) {
    let mut state = lifecycle.lock().unwrap();
    if epoch_counter.load(Ordering::SeqCst) != epoch {
        return;
    }
    if !response_has_ended(&state, epoch, response_id) {
        state.ended_response = Some((epoch, response_id));
    }
    if state.latest_response == Some((epoch, response_id)) {
        playing.store(false, Ordering::SeqCst);
        active.store(false, Ordering::SeqCst);
        if active_response.load(Ordering::SeqCst) == response_id {
            active_response.store(0, Ordering::SeqCst);
        }
    }
}

/// Audio Playback Controller supporting native Station Mini `mini_pcm_sink`
/// (MPCM protocol, S16_LE stereo 48kHz) with fallback to `aplay`,
/// generation-based queue cancellation, and immediate barge-in interruption.
pub struct AudioPlayback {
    audio_tx: mpsc::Sender<(u64, u64, Vec<u8>)>,
    cancellation_epoch: Arc<AtomicU64>,
    pub active_response_id: Arc<AtomicU64>,
    current_child: Arc<Mutex<Option<Child>>>,
    is_playing: Arc<AtomicBool>,
    has_active_generation: Arc<AtomicBool>,
    lifecycle: Arc<Mutex<PlaybackLifecycle>>,
}

impl AudioPlayback {
    /// Start the playback worker on the specified ALSA device (e.g. "default" or "hw:0,0").
    /// Accepts raw 24kHz 16-bit mono PCM chunks from Gemini Live.
    pub fn start(device_name: String, sample_rate: u32) -> Result<Self> {
        let backend = PlaybackBackend::detect(&device_name, sample_rate);
        if !cfg!(test) && matches!(backend, PlaybackBackend::Mock) {
            anyhow::bail!("No real audio output backend found (mini_pcm_sink/aplay); refusing silent production playback");
        }
        Self::start_with_backend(device_name, sample_rate, backend)
    }

    /// Start the playback worker with an explicit backend (useful for tests and dry-run).
    pub fn start_with_backend(
        _device_name: String,
        _sample_rate: u32,
        backend: PlaybackBackend,
    ) -> Result<Self> {
        let (audio_tx, mut audio_rx) = mpsc::channel::<(u64, u64, Vec<u8>)>(100);
        let cancellation_epoch = Arc::new(AtomicU64::new(0));
        let cancellation_epoch_for_worker = cancellation_epoch.clone();
        let active_response_id = Arc::new(AtomicU64::new(0));
        let active_response_for_worker = active_response_id.clone();
        let current_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let current_child_for_worker = current_child.clone();
        let is_playing = Arc::new(AtomicBool::new(false));
        let is_playing_for_worker = is_playing.clone();
        let has_active_generation = Arc::new(AtomicBool::new(false));
        let has_active_generation_for_worker = has_active_generation.clone();
        let lifecycle = Arc::new(Mutex::new(PlaybackLifecycle::default()));
        let lifecycle_for_worker = lifecycle.clone();

        info!("Audio playback backend detected: {:?}", backend);

        std::thread::spawn(move || {
            let mut local_stdin: Option<ChildStdin> = None;
            let mut last_sample: Option<i16> = None;

            while let Some((epoch, resp_id, chunk)) = audio_rx.blocking_recv() {
                {
                    let mut state = lifecycle_for_worker.lock().unwrap();
                    if epoch != cancellation_epoch_for_worker.load(Ordering::SeqCst)
                        || response_has_ended(&state, epoch, resp_id)
                    {
                        continue;
                    }
                    if chunk.is_empty() {
                        state.ended_response = Some((epoch, resp_id));
                    }
                }

                // END closes input; completion follows actual process drain, not a timer.
                if chunk.is_empty() {
                    if let Some(ref mut stdin) = local_stdin {
                        if matches!(backend, PlaybackBackend::MiniPcmSink { .. }) {
                            let end_hdr = make_mpcm_record_header(2, 0); // Type 2 = END
                            let _ = stdin.write_all(&end_hdr);
                            let _ = stdin.flush();
                        }
                    }
                    local_stdin = None;
                    last_sample = None;
                    let is_playing_drain = is_playing_for_worker.clone();
                    let has_active_drain = has_active_generation_for_worker.clone();
                    let active_resp_drain = active_response_for_worker.clone();
                    let epoch_drain = cancellation_epoch_for_worker.clone();
                    let lifecycle_drain = lifecycle_for_worker.clone();
                    let child_drain = current_child_for_worker.clone();
                    let target_pid = child_drain.lock().unwrap().as_ref().map(Child::id);
                    if let Some(target_pid) = target_pid {
                        std::thread::spawn(move || {
                            // Native sink has a 330s session deadline; bound a broken drain too.
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(345);
                            loop {
                                let finished = {
                                    let mut slot = child_drain.lock().unwrap();
                                    match slot.as_mut() {
                                        Some(child) if child.id() == target_pid => {
                                            match child.try_wait() {
                                                Ok(Some(_)) | Err(_) => {
                                                    *slot = None;
                                                    true
                                                }
                                                Ok(None)
                                                    if std::time::Instant::now() >= deadline =>
                                                {
                                                    let _ = child.kill();
                                                    let _ = child.wait();
                                                    *slot = None;
                                                    true
                                                }
                                                Ok(None) => false,
                                            }
                                        }
                                        _ => true,
                                    }
                                };
                                if finished {
                                    finish_playback_response(
                                        &lifecycle_drain,
                                        &epoch_drain,
                                        &active_resp_drain,
                                        &is_playing_drain,
                                        &has_active_drain,
                                        epoch,
                                        resp_id,
                                    );
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                        });
                    } else {
                        // Mock/idle output has no kernel drain or child to reap.
                        finish_playback_response(
                            &lifecycle_drain,
                            &epoch_drain,
                            &active_resp_drain,
                            &is_playing_drain,
                            &has_active_drain,
                            epoch,
                            resp_id,
                        );
                    }
                    continue;
                }

                // A new response waits for the previous renderer to drain. Its Child
                // remains owned and interruptible until it has actually exited.
                while local_stdin.is_none() {
                    if cancellation_epoch_for_worker.load(Ordering::SeqCst) != epoch {
                        break;
                    }
                    let busy = {
                        let mut slot = current_child_for_worker.lock().unwrap();
                        match slot.as_mut() {
                            Some(child) => match child.try_wait() {
                                Ok(None) => true,
                                Ok(Some(_)) | Err(_) => {
                                    *slot = None;
                                    false
                                }
                            },
                            None => false,
                        }
                    };
                    if !busy {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }

                // A renderer dying before END fails this response. Restarting it
                // on the next chunk would speak a disconnected middle fragment.
                let mut renderer_failed = false;
                let need_spawn = {
                    let mut lock = current_child_for_worker.lock().unwrap();
                    match lock.as_mut() {
                        None => true,
                        Some(c) => match c.try_wait() {
                            Ok(Some(_)) | Err(_) => {
                                *lock = None;
                                local_stdin = None;
                                last_sample = None;
                                renderer_failed = true;
                                false
                            }
                            Ok(None) => local_stdin.is_none(),
                        },
                    }
                };

                if renderer_failed {
                    finish_playback_response(
                        &lifecycle_for_worker,
                        &cancellation_epoch_for_worker,
                        &active_response_for_worker,
                        &is_playing_for_worker,
                        &has_active_generation_for_worker,
                        epoch,
                        resp_id,
                    );
                    continue;
                }

                if need_spawn {
                    if cancellation_epoch_for_worker.load(Ordering::SeqCst) != epoch {
                        continue;
                    }

                    match &backend {
                        PlaybackBackend::MiniPcmSink { path } => {
                            let mut cmd = Command::new(path);
                            cmd.stdin(Stdio::piped())
                                .stdout(Stdio::piped())
                                .stderr(Stdio::inherit());

                            if Path::new("/system/vendor/quasar/libs").exists() {
                                let current_ld =
                                    std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
                                let new_ld = if current_ld.is_empty() {
                                    "/system/vendor/quasar/libs".to_string()
                                } else {
                                    format!("{}:/system/vendor/quasar/libs", current_ld)
                                };
                                cmd.env("LD_LIBRARY_PATH", new_ld);
                            }

                            match cmd.spawn() {
                                Ok(mut child) => {
                                    let _state = lifecycle_for_worker.lock().unwrap();
                                    let mut lock = current_child_for_worker.lock().unwrap();
                                    if cancellation_epoch_for_worker.load(Ordering::SeqCst) != epoch
                                    {
                                        let _ = child.kill();
                                        let _ = child.wait();
                                        continue;
                                    }
                                    if let Some(stdout) = child.stdout.take() {
                                        let epoch_mon = epoch;
                                        let resp_mon = resp_id;
                                        std::thread::spawn(move || {
                                            use std::io::BufRead;
                                            let reader = std::io::BufReader::new(stdout);
                                            for line in reader.lines().map_while(Result::ok) {
                                                if line.contains("mini.pcm_sink.v1") {
                                                    if let Ok(val) =
                                                        serde_json::from_str::<serde_json::Value>(
                                                            &line,
                                                        )
                                                    {
                                                        let status = val
                                                            .get("status")
                                                            .and_then(|s| s.as_str())
                                                            .unwrap_or("unknown");
                                                        let stage = val
                                                            .get("stage")
                                                            .and_then(|s| s.as_str())
                                                            .unwrap_or("unknown");
                                                        let accepted = val
                                                            .get("accepted_audio_frames")
                                                            .and_then(|v| v.as_u64())
                                                            .unwrap_or(0);
                                                        let dropped = val
                                                            .get("dropped_queued_frames")
                                                            .and_then(|v| v.as_u64())
                                                            .unwrap_or(0);
                                                        info!(
                                                            "mini_pcm_sink report [ep:{} resp:{}]: status={}, stage={}, accepted_frames={}, dropped={}",
                                                            epoch_mon, resp_mon, status, stage, accepted, dropped
                                                        );
                                                    }
                                                }
                                            }
                                        });
                                    }
                                    if let Some(mut stdin) = child.stdin.take() {
                                        let preamble = make_mpcm_preamble();
                                        if let Err(e) = stdin.write_all(&preamble) {
                                            debug!("Failed to write MPCM preamble: {}", e);
                                            let _ = child.kill();
                                            let _ = child.wait();
                                            drop(lock);
                                            drop(_state);
                                            finish_playback_response(
                                                &lifecycle_for_worker,
                                                &cancellation_epoch_for_worker,
                                                &active_response_for_worker,
                                                &is_playing_for_worker,
                                                &has_active_generation_for_worker,
                                                epoch,
                                                resp_id,
                                            );
                                            continue;
                                        }
                                        local_stdin = Some(stdin);
                                    }
                                    *lock = Some(child);
                                    last_sample = None;
                                    info!(
                                        "AudioPlayback: started mini_pcm_sink for epoch {} response {}",
                                        epoch, resp_id
                                    );
                                }
                                Err(e) => {
                                    debug!(
                                        "Failed to spawn mini_pcm_sink ({}): {}",
                                        path.display(),
                                        e
                                    );
                                    finish_playback_response(
                                        &lifecycle_for_worker,
                                        &cancellation_epoch_for_worker,
                                        &active_response_for_worker,
                                        &is_playing_for_worker,
                                        &has_active_generation_for_worker,
                                        epoch,
                                        resp_id,
                                    );
                                    continue;
                                }
                            }
                        }
                        PlaybackBackend::Aplay {
                            path,
                            device,
                            sample_rate,
                        } => {
                            let mut cmd = Command::new(path);
                            cmd.args([
                                "-q",
                                "-D",
                                device,
                                "-r",
                                &sample_rate.to_string(),
                                "-f",
                                "S16_LE",
                                "-c",
                                "1",
                                "-t",
                                "raw",
                            ])
                            .stdin(Stdio::piped())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null());

                            if Path::new("/system/vendor/quasar/libs").exists() {
                                let current_ld =
                                    std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
                                let new_ld = if current_ld.is_empty() {
                                    "/system/vendor/quasar/libs".to_string()
                                } else {
                                    format!("{}:/system/vendor/quasar/libs", current_ld)
                                };
                                cmd.env("LD_LIBRARY_PATH", new_ld);
                            }

                            match cmd.spawn() {
                                Ok(mut child) => {
                                    let _state = lifecycle_for_worker.lock().unwrap();
                                    let mut lock = current_child_for_worker.lock().unwrap();
                                    if cancellation_epoch_for_worker.load(Ordering::SeqCst) != epoch
                                    {
                                        let _ = child.kill();
                                        let _ = child.wait();
                                        continue;
                                    }
                                    local_stdin = child.stdin.take();
                                    *lock = Some(child);
                                    info!(
                                        "AudioPlayback: started aplay for epoch {} response {}",
                                        epoch, resp_id
                                    );
                                }
                                Err(e) => {
                                    debug!(
                                        "Mock playback (aplay failed: {}), {} bytes",
                                        e,
                                        chunk.len()
                                    );
                                    finish_playback_response(
                                        &lifecycle_for_worker,
                                        &cancellation_epoch_for_worker,
                                        &active_response_for_worker,
                                        &is_playing_for_worker,
                                        &has_active_generation_for_worker,
                                        epoch,
                                        resp_id,
                                    );
                                    continue;
                                }
                            }
                        }
                        PlaybackBackend::Mock => {
                            let _state = lifecycle_for_worker.lock().unwrap();
                            if cancellation_epoch_for_worker.load(Ordering::SeqCst) != epoch {
                                continue;
                            }
                            debug!(
                                "Mock playback: received {} bytes for epoch {} resp {}",
                                chunk.len(),
                                epoch,
                                resp_id
                            );
                            active_response_for_worker.store(resp_id, Ordering::SeqCst);
                            is_playing_for_worker.store(true, Ordering::SeqCst);
                            continue;
                        }
                    }
                }

                // Write audio chunk
                if let Some(ref mut stdin) = local_stdin {
                    {
                        let _state = lifecycle_for_worker.lock().unwrap();
                        if cancellation_epoch_for_worker.load(Ordering::SeqCst) != epoch {
                            continue;
                        }
                        active_response_for_worker.store(resp_id, Ordering::SeqCst);
                        is_playing_for_worker.store(true, Ordering::SeqCst);
                    }
                    let write_res = match &backend {
                        PlaybackBackend::MiniPcmSink { .. } => {
                            let stereo_48k =
                                convert_24k_mono_to_48k_stereo(&chunk, &mut last_sample);
                            let mut write_err = None;
                            for record_slice in stereo_48k.chunks(4096) {
                                let hdr = make_mpcm_record_header(1, record_slice.len() as u32);
                                if let Err(e) = stdin.write_all(&hdr) {
                                    write_err = Some(e);
                                    break;
                                }
                                if let Err(e) = stdin.write_all(record_slice) {
                                    write_err = Some(e);
                                    break;
                                }
                            }
                            if write_err.is_none() {
                                let _ = stdin.flush();
                            }
                            write_err.map(Err).unwrap_or(Ok(()))
                        }
                        PlaybackBackend::Aplay { .. } => stdin.write_all(&chunk),
                        PlaybackBackend::Mock => Ok(()),
                    };

                    if let Err(e) = write_res {
                        debug!("Failed to write chunk to playback: {}", e);
                        finish_playback_response(
                            &lifecycle_for_worker,
                            &cancellation_epoch_for_worker,
                            &active_response_for_worker,
                            &is_playing_for_worker,
                            &has_active_generation_for_worker,
                            epoch,
                            resp_id,
                        );
                        local_stdin = None;
                        last_sample = None;
                        let mut lock = current_child_for_worker.lock().unwrap();
                        if let Some(mut child) = lock.take() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            }

            let mut lock = current_child_for_worker.lock().unwrap();
            if let Some(mut child) = lock.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        });

        Ok(Self {
            audio_tx,
            cancellation_epoch,
            active_response_id,
            current_child,
            is_playing,
            has_active_generation,
            lifecycle,
        })
    }

    /// Queue audio chunk for playback tagged with its cancellation epoch and response ID.
    /// If the epoch is older than current active cancellation epoch (e.g. from an interrupted turn),
    /// it is discarded immediately before entering the worker queue.
    pub async fn play(&self, epoch: u64, response_id: u64, pcm_chunk: Vec<u8>) -> Result<()> {
        // Never hold a synchronous lock while waiting for queue capacity.
        // The reserved send is synchronous, so interrupt cannot race between
        // admission validation and marking queued output as active.
        let permit = self
            .audio_tx
            .reserve()
            .await
            .context("Playback channel closed")?;
        let mut state = self.lifecycle.lock().unwrap();
        let active_epoch = self.cancellation_epoch.load(Ordering::SeqCst);
        let source_matches = if response_id == ALARM_RESPONSE_ID {
            state.alarm_epoch == Some(epoch)
        } else {
            state.alarm_epoch.is_none()
        };
        if !source_matches || epoch != active_epoch {
            return Ok(());
        }
        if response_has_ended(&state, epoch, response_id) {
            if response_id == ALARM_RESPONSE_ID {
                anyhow::bail!("Alarm renderer has ended");
            }
            return Ok(());
        }
        if !pcm_chunk.is_empty() {
            state.latest_response = Some((epoch, response_id));
            self.has_active_generation.store(true, Ordering::SeqCst);
            self.is_playing.store(true, Ordering::SeqCst);
        }
        permit.send((epoch, response_id, pcm_chunk));
        Ok(())
    }

    /// Immediately interrupt playback (barge-in when user speaks).
    /// Kills active child and invalidates all queued chunks from earlier epochs.
    pub fn interrupt(&self) {
        let mut state = self.lifecycle.lock().unwrap();
        if state.alarm_epoch.is_none() {
            self.interrupt_locked(&mut state);
        }
    }

    /// Cancel only the specified epoch. A timeout for an earlier turn must not
    /// clear flags or terminate the renderer of a newer turn.
    pub fn interrupt_if_epoch(&self, expected: u64) -> bool {
        let mut state = self.lifecycle.lock().unwrap();
        if state.alarm_epoch.is_some() || self.cancellation_epoch.load(Ordering::SeqCst) != expected
        {
            return false;
        }
        self.interrupt_locked(&mut state);
        true
    }

    /// Preempt voice and reserve exclusive output until this alarm lease is released.
    pub fn claim_alarm(&self) -> u64 {
        let mut state = self.lifecycle.lock().unwrap();
        self.interrupt_locked(&mut state);
        let epoch = self.cancellation_epoch();
        state.alarm_epoch = Some(epoch);
        epoch
    }

    /// Release only the matching alarm lease and discard its pending audio.
    pub fn release_alarm(&self, epoch: u64) -> bool {
        let mut state = self.lifecycle.lock().unwrap();
        if state.alarm_epoch != Some(epoch) {
            return false;
        }
        state.alarm_epoch = None;
        self.interrupt_locked(&mut state);
        true
    }

    fn interrupt_locked(&self, state: &mut PlaybackLifecycle) {
        state.latest_response = None;
        let new_epoch = self.cancellation_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.active_response_id.store(0, Ordering::SeqCst);
        self.is_playing.store(false, Ordering::SeqCst);
        self.has_active_generation.store(false, Ordering::SeqCst);
        if let Ok(mut lock) = self.current_child.lock() {
            if let Some(mut child) = lock.take() {
                debug!(
                    "Barge-in interrupt: killing active playback process (new epoch: {})",
                    new_epoch
                );
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Access the shared cancellation epoch handle
    pub fn cancellation_epoch_handle(&self) -> Arc<AtomicU64> {
        self.cancellation_epoch.clone()
    }

    /// Check if audio playback is currently active (used to suppress speaker echo in mic).
    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::SeqCst)
    }

    /// Current cancellation epoch counter
    pub fn cancellation_epoch(&self) -> u64 {
        self.cancellation_epoch.load(Ordering::SeqCst)
    }

    /// Legacy generation alias for tests
    #[allow(dead_code)]
    pub fn generation(&self) -> u64 {
        self.cancellation_epoch()
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        let mut state = self.lifecycle.lock().unwrap();
        self.interrupt_locked(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn alarm_lease_excludes_voice_and_stale_releases() {
        let playback =
            AudioPlayback::start_with_backend("default".into(), 24000, PlaybackBackend::Mock)
                .unwrap();
        let earlier = playback.claim_alarm();
        let epoch = playback.claim_alarm();
        assert!(!playback.release_alarm(earlier));
        playback.interrupt();
        assert!(!playback.interrupt_if_epoch(epoch));
        playback.play(epoch, 1, vec![0; 100]).await.unwrap();
        assert!(!playback.is_playing());
        playback
            .play(epoch, ALARM_RESPONSE_ID, vec![0; 100])
            .await
            .unwrap();
        assert!(playback.is_playing());
        assert!(playback.release_alarm(epoch));
        let epoch = playback.cancellation_epoch();
        playback.play(epoch, 2, vec![0; 100]).await.unwrap();
        assert!(playback.is_playing());
    }

    #[tokio::test]
    async fn test_playback_generation_increments_on_interrupt() {
        let playback = AudioPlayback::start("default".to_string(), 24000).unwrap();
        assert_eq!(playback.generation(), 0);

        playback.play(0, 1, vec![0u8; 100]).await.unwrap();
        assert_eq!(playback.generation(), 0);

        playback.interrupt();
        assert_eq!(playback.generation(), 1);

        playback.interrupt();
        assert_eq!(playback.generation(), 2);
    }

    #[tokio::test]
    async fn test_playback_stale_chunks_rejected_after_interrupt() {
        let playback = AudioPlayback::start("default".to_string(), 24000).unwrap();
        // Play epoch 0 response 1
        playback.play(0, 1, vec![1, 2, 3]).await.unwrap();
        playback.interrupt(); // epoch moves to 1

        // Attempt to play under old epoch 0: rejected immediately
        playback.play(0, 1, vec![4, 5, 6]).await.unwrap();

        // Send chunk under old epoch directly into internal channel: worker skips it
        playback.audio_tx.send((0, 1, vec![4, 5, 6])).await.unwrap();

        // New play will be under epoch 1 response 2
        playback.play(1, 2, vec![7, 8, 9]).await.unwrap();
        assert_eq!(playback.generation(), 1);
    }

    #[test]
    fn test_mpcm_preamble_structure() {
        let preamble = make_mpcm_preamble();
        assert_eq!(&preamble[0..4], b"MPCM");
        assert_eq!(u32::from_le_bytes(preamble[4..8].try_into().unwrap()), 1); // version 1
        assert_eq!(
            u32::from_le_bytes(preamble[8..12].try_into().unwrap()),
            48000
        ); // 48kHz
        assert_eq!(u16::from_le_bytes(preamble[12..14].try_into().unwrap()), 2); // stereo
        assert_eq!(u16::from_le_bytes(preamble[14..16].try_into().unwrap()), 16);
        // 16-bit
    }

    #[test]
    fn test_mpcm_record_headers() {
        let data_hdr = make_mpcm_record_header(1, 4096);
        assert_eq!(u32::from_le_bytes(data_hdr[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(data_hdr[4..8].try_into().unwrap()), 4096);

        let end_hdr = make_mpcm_record_header(2, 0);
        assert_eq!(u32::from_le_bytes(end_hdr[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(end_hdr[4..8].try_into().unwrap()), 0);

        let cancel_hdr = make_mpcm_record_header(3, 0);
        assert_eq!(u32::from_le_bytes(cancel_hdr[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(cancel_hdr[4..8].try_into().unwrap()), 0);
    }

    #[test]
    fn test_convert_24k_mono_to_48k_stereo_upsampling_and_clamping() {
        let mut last_sample = None;
        // 2 samples: 1000, 3000
        let s0 = 1000i16;
        let s1 = 3000i16;
        let mut input = Vec::new();
        input.extend_from_slice(&s0.to_le_bytes());
        input.extend_from_slice(&s1.to_le_bytes());

        let out = convert_24k_mono_to_48k_stereo(&input, &mut last_sample);
        // 2 mono samples -> 4 stereo frames = 8 samples = 16 bytes
        assert_eq!(out.len(), 16);

        // Frame 0: s_mid = 1000, left=1000, right=1000
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 1000);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 1000);
        // Frame 1: s_curr = 1000, left=1000, right=1000
        assert_eq!(i16::from_le_bytes([out[4], out[5]]), 1000);
        assert_eq!(i16::from_le_bytes([out[6], out[7]]), 1000);
        // Frame 2: s_mid = (1000+3000)/2 = 2000, left=2000, right=2000
        assert_eq!(i16::from_le_bytes([out[8], out[9]]), 2000);
        assert_eq!(i16::from_le_bytes([out[10], out[11]]), 2000);
        // Frame 3: s_curr = 3000, left=3000, right=3000
        assert_eq!(i16::from_le_bytes([out[12], out[13]]), 3000);
        assert_eq!(i16::from_le_bytes([out[14], out[15]]), 3000);

        // Test peak clamping
        let mut clamp_input = Vec::new();
        clamp_input.extend_from_slice(&32000i16.to_le_bytes());
        clamp_input.extend_from_slice(&(-30000i16).to_le_bytes());
        let clamped_out = convert_24k_mono_to_48k_stereo(&clamp_input, &mut last_sample);
        // Checked against 16000 ceiling
        let max_sample = clamped_out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .max()
            .unwrap();
        let min_sample = clamped_out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .min()
            .unwrap();
        assert!(max_sample <= 16000);
        assert!(min_sample >= -16000);
    }

    #[tokio::test]
    async fn test_empty_chunk_does_not_spawn_child() {
        let playback = AudioPlayback::start("default".to_string(), 24000).unwrap();
        // Sending empty chunk (end sentinel) on idle playback should be completely benign
        playback.play(0, 1, Vec::new()).await.unwrap();
        assert_eq!(playback.generation(), 0);
    }

    #[tokio::test]
    async fn test_old_end_timer_must_not_unmute_newer_turn() {
        let playback = AudioPlayback::start("default".to_string(), 24000).unwrap();
        // Turn 0 plays audio (epoch 0, resp 1)
        playback.play(0, 1, vec![1, 2, 3, 4]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Turn 0 finishes, triggering 350ms drain timer for resp 1
        playback.play(0, 1, Vec::new()).await.unwrap();

        // Turn 1 starts shortly after (same epoch 0, next resp 2)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        playback.play(0, 2, vec![5, 6, 7, 8]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(playback.is_playing());

        // Wait for Turn 0's 350ms drain timer to fire
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;

        // Regression test for R3: Turn 0's timer must not clear is_playing for Turn 1
        assert!(
            playback.is_playing(),
            "Turn 0 drain timer erroneously unmuted Turn 1"
        );
    }

    #[tokio::test]
    async fn test_consecutive_responses_same_epoch_distinct_ids_keep_end_and_do_not_race() {
        let playback = AudioPlayback::start("default".to_string(), 24000).unwrap();
        // Normal turn A (epoch 0, resp 1)
        playback.play(0, 1, vec![1, 2, 3]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(playback.is_playing());

        // Turn A END
        playback.play(0, 1, Vec::new()).await.unwrap();

        // Drain finishes for turn A
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !playback.is_playing(),
            "Playback should be idle after Turn A drains"
        );

        // Normal turn B (same epoch 0, distinct resp 2)
        playback.play(0, 2, vec![4, 5, 6]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(playback.is_playing(), "Turn B must set is_playing");

        // Turn B END
        playback.play(0, 2, Vec::new()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !playback.is_playing(),
            "Playback should be idle after Turn B drains"
        );
    }

    #[tokio::test]
    async fn test_old_epoch_timeout_cannot_cancel_newer_playback() {
        let playback =
            AudioPlayback::start_with_backend("mock".to_string(), 24000, PlaybackBackend::Mock)
                .unwrap();
        assert!(playback.interrupt_if_epoch(0));
        playback.play(1, 2, vec![0; 32]).await.unwrap();
        wait_for(|| playback.active_response_id.load(Ordering::SeqCst) == 2).await;
        assert!(!playback.interrupt_if_epoch(0));
        assert!(playback.is_playing());
        assert_eq!(playback.cancellation_epoch(), 1);
        assert!(playback.interrupt_if_epoch(1));
        assert!(!playback.is_playing());
    }

    #[tokio::test]
    async fn test_unknown_future_epoch_never_activates_playback() {
        let playback =
            AudioPlayback::start_with_backend("mock".to_string(), 24000, PlaybackBackend::Mock)
                .unwrap();
        playback.play(1, 1, vec![0; 32]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!playback.is_playing());
        assert_eq!(playback.active_response_id.load(Ordering::SeqCst), 0);
        assert_eq!(playback.cancellation_epoch(), 0);
    }

    #[tokio::test]
    async fn test_audio_after_completed_response_cannot_restart_it() {
        let playback =
            AudioPlayback::start_with_backend("mock".to_string(), 24000, PlaybackBackend::Mock)
                .unwrap();
        playback.play(0, 1, vec![0; 32]).await.unwrap();
        wait_for(|| playback.active_response_id.load(Ordering::SeqCst) == 1).await;
        playback.play(0, 1, Vec::new()).await.unwrap();
        wait_for(|| !playback.is_playing()).await;
        playback.play(0, 1, vec![0; 32]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!playback.is_playing());
        assert_eq!(playback.active_response_id.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_cancel_serializes_with_reserved_play_admission() {
        let playback = Arc::new(
            AudioPlayback::start_with_backend("mock".to_string(), 24000, PlaybackBackend::Mock)
                .unwrap(),
        );
        // Pause admission after capacity can be reserved, before state changes.
        let guard = playback.lifecycle.lock().unwrap();
        let other = playback.clone();
        let sender = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(other.play(0, 1, vec![0; 32]))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while playback.audio_tx.capacity() == 100 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let reserved = playback.audio_tx.capacity() == 99;
        let active_while_blocked = playback.is_playing();
        drop(guard);
        playback.interrupt();
        sender.join().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(reserved, "the fixture must reach the admission boundary");
        assert!(
            !active_while_blocked,
            "waiting admission cannot activate the mic gate"
        );
        assert!(
            !playback.is_playing(),
            "cancelled admission must never leave playback active"
        );
        assert_eq!(playback.active_response_id.load(Ordering::SeqCst), 0);
    }

    async fn wait_for(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !predicate() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("bounded fake-renderer observation");
    }

    #[cfg(unix)]
    struct FakeDrainRenderer {
        directory: PathBuf,
        executable: PathBuf,
        events: PathBuf,
    }

    #[cfg(unix)]
    impl FakeDrainRenderer {
        fn new(drain_seconds: f32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "mini-playback-drain-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst),
            ));
            std::fs::create_dir(&directory).unwrap();
            let executable = directory.join("fake-renderer.py");
            let events = directory.join("events.txt");
            let events_literal = serde_json::to_string(events.to_str().unwrap()).unwrap();
            std::fs::write(
                &executable,
                format!(
                    "#!/usr/bin/env python3\nimport sys,time\np={events_literal}\n\
                 def event(s):\n    with open(p,'a') as f: f.write(s+'\\n')\n\
                 event('start')\nsys.stdin.buffer.read()\nevent('eof')\n\
                 time.sleep({drain_seconds})\nevent('exit')\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                directory,
                executable,
                events,
            }
        }

        fn events(&self) -> Vec<String> {
            std::fs::read_to_string(&self.events)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeDrainRenderer {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_end_waits_for_real_drain_and_serializes_next_renderer() {
        let fixture = FakeDrainRenderer::new(0.7);
        let playback = AudioPlayback::start_with_backend(
            "fake".to_string(),
            24000,
            PlaybackBackend::MiniPcmSink {
                path: fixture.executable.clone(),
            },
        )
        .unwrap();
        playback.play(0, 1, vec![0; 64]).await.unwrap();
        wait_for(|| fixture.events().iter().any(|s| s == "start")).await;
        playback.play(0, 1, Vec::new()).await.unwrap();
        wait_for(|| fixture.events().iter().any(|s| s == "eof")).await;
        tokio::time::sleep(std::time::Duration::from_millis(425)).await;
        assert!(
            playback.is_playing(),
            "END is not an arbitrary 350ms drain timer"
        );
        playback.play(0, 2, vec![0; 64]).await.unwrap();
        playback.play(0, 2, Vec::new()).await.unwrap();
        wait_for(|| fixture.events().iter().filter(|s| *s == "start").count() == 2).await;
        let events = fixture.events();
        let first_exit = events.iter().position(|s| s == "exit").unwrap();
        let second_start = events.iter().rposition(|s| s == "start").unwrap();
        assert!(
            first_exit < second_start,
            "a new renderer must not overlap the previous drain"
        );
        wait_for(|| !playback.is_playing()).await;
        assert_eq!(fixture.events().iter().filter(|s| *s == "exit").count(), 2);
        assert!(playback.current_child.lock().unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_interrupt_reaps_a_draining_renderer() {
        let fixture = FakeDrainRenderer::new(5.0);
        let playback = AudioPlayback::start_with_backend(
            "fake".to_string(),
            24000,
            PlaybackBackend::MiniPcmSink {
                path: fixture.executable.clone(),
            },
        )
        .unwrap();
        playback.play(0, 1, vec![0; 64]).await.unwrap();
        wait_for(|| fixture.events().iter().any(|s| s == "start")).await;
        playback.play(0, 1, Vec::new()).await.unwrap();
        wait_for(|| fixture.events().iter().any(|s| s == "eof")).await;
        assert!(playback.interrupt_if_epoch(0));
        assert!(!playback.is_playing());
        assert!(playback.current_child.lock().unwrap().is_none());
        assert!(!fixture.events().iter().any(|s| s == "exit"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_failed_renderer_does_not_restart_mid_response() {
        let fixture = FakeDrainRenderer::new(0.0);
        let source = std::fs::read_to_string(&fixture.executable).unwrap();
        std::fs::write(
            &fixture.executable,
            source.replace("sys.stdin.buffer.read()", "sys.stdin.buffer.read(16)"),
        )
        .unwrap();
        let playback = AudioPlayback::start_with_backend(
            "fake".to_string(),
            24000,
            PlaybackBackend::MiniPcmSink {
                path: fixture.executable.clone(),
            },
        )
        .unwrap();
        playback.play(0, 1, vec![0; 64]).await.unwrap();
        wait_for(|| fixture.events().iter().any(|s| s == "exit")).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        playback.play(0, 1, vec![0; 64]).await.unwrap();
        wait_for(|| !playback.is_playing()).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            fixture.events().iter().filter(|s| *s == "start").count(),
            1,
            "a failed renderer must not restart with a middle fragment of the same reply"
        );
        assert!(playback.current_child.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn audit_local_barge_then_cloud_interrupt_accepts_next_model_pcm() {
        use crate::gemini::{GeminiLiveClient, LiveClientConfig, UserAudio};
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio_tungstenite::tungstenite::Message;

        const PCM: &str = r#"{"serverContent":{"modelTurn":{"parts":[{"inlineData":{"mimeType":"audio/pcm;rate=24000","data":"AQABAAEAAQA="}}]}}}"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bounded loopback bind (environment restriction is not app failure)");
        let addr = listener.local_addr().unwrap();
        let authorization = Arc::new(crate::interaction::TurnAuthorization::new(
            Duration::from_secs(8),
        ));
        let mut server = tokio::spawn(async move {
            let session = async move {
                let (stream, _) = listener.accept().await?;
                let mut ws = tokio_tungstenite::accept_async(stream).await?;
                let setup = ws
                    .next()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("missing setup"))??;
                anyhow::ensure!(matches!(setup, Message::Text(_)), "expected text setup");
                ws.send(Message::Text(r#"{"setupComplete":{}}"#.to_string()))
                    .await?;
                for turn in 0..2 {
                    // Only real synthetic user input ending on the wire can
                    // authorize this response; local permit alone is not input.
                    loop {
                        let msg = ws
                            .next()
                            .await
                            .ok_or_else(|| anyhow::anyhow!("input ended too soon"))??;
                        if let Message::Text(text) = msg {
                            if serde_json::from_str::<serde_json::Value>(&text)?
                                .pointer("/realtimeInput/activityEnd")
                                .is_some()
                            {
                                break;
                            }
                        }
                    }
                    if turn == 1 {
                        ws.send(Message::Text(
                            r#"{"serverContent":{"interrupted":true}}"#.to_string(),
                        ))
                        .await?;
                    }
                    ws.send(Message::Text(PCM.to_string())).await?;
                }
                while let Some(msg) = ws.next().await {
                    if matches!(msg?, Message::Close(_)) {
                        break;
                    }
                }
                anyhow::Ok(())
            };
            tokio::time::timeout(Duration::from_secs(8), session).await
        });

        let playback = Arc::new(
            AudioPlayback::start_with_backend(
                "not-a-device".to_string(),
                24000,
                PlaybackBackend::Mock,
            )
            .unwrap(),
        );
        let playback_for_callback = playback.clone();
        let callback = Arc::new(move || playback_for_callback.interrupt());
        let (tx_user, rx_user) = mpsc::channel(4);
        let (tx_model, mut rx_model) = mpsc::channel(4);
        let cfg = LiveClientConfig {
            api_key: "TEST_ONLY_UNUSED_ON_LOOPBACK".to_string(),
            model: "models/loopback-fixture".to_string(),
            voice_name: "Aoede".to_string(),
            system_instruction: "test fixture".to_string(),
            has_ha_tools: false,
            has_weather_tools: false,
            has_alarm_tools: false,
            has_spotify_tools: false,
            has_stop_dialogue: false,
            endpoint: Some(format!("ws://{addr}/ws")),
            cancellation_epoch: Some(playback.cancellation_epoch_handle()),
            authorization: Some(authorization.clone()),
            suppress_followup: None,
        };
        let mut client = None;
        let execution: anyhow::Result<serde_json::Value> =
            match tokio::time::timeout(Duration::from_secs(6), async {
                client =
                    Some(GeminiLiveClient::connect(cfg, rx_user, tx_model, None, callback).await?);
                let first = authorization.begin_explicit_turn();
                tx_user
                    .send(UserAudio {
                        turn_id: first,
                        pcm: vec![0; 32],
                    })
                    .await?;
                tx_user
                    .send(UserAudio {
                        turn_id: first,
                        pcm: Vec::new(),
                    })
                    .await?;
                let (initial_epoch, initial_id, initial_pcm) = rx_model
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("missing initial model PCM"))?;
                playback
                    .play(initial_epoch, initial_id, initial_pcm)
                    .await?;
                tokio::time::timeout(Duration::from_secs(1), async {
                    while playback.active_response_id.load(Ordering::SeqCst) != initial_id {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                })
                .await
                .map_err(|_| anyhow::anyhow!("initial PCM was not accepted by mock worker"))?;
                anyhow::ensure!(
                    playback.is_playing(),
                    "initial mock playback must be active"
                );

                playback.interrupt();
                let after_local_epoch = playback.cancellation_epoch();
                authorization.cancel();
                let next = authorization.begin_explicit_turn();
                tx_user
                    .send(UserAudio {
                        turn_id: next,
                        pcm: vec![0; 32],
                    })
                    .await?;
                tx_user
                    .send(UserAudio {
                        turn_id: next,
                        pcm: Vec::new(),
                    })
                    .await?;
                let (next_epoch, next_id, next_pcm) = rx_model
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("missing next model PCM"))?;
                let after_cloud_epoch = playback.cancellation_epoch();
                playback.play(next_epoch, next_id, next_pcm).await?;
                let accepted = tokio::time::timeout(Duration::from_millis(500), async {
                    while playback.active_response_id.load(Ordering::SeqCst) != next_id
                        || !playback.is_playing()
                    {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                })
                .await
                .is_ok();
                anyhow::Ok(serde_json::json!({
                    "initial_epoch":initial_epoch,"initial_response_id":initial_id,
                    "after_local_epoch":after_local_epoch,"after_cloud_epoch":after_cloud_epoch,
                    "next_model_epoch":next_epoch,"next_response_id":next_id,
                    "next_pcm_accepted_by_mock_worker":accepted,
                    "playback_is_playing":playback.is_playing(),
                    "worker_response_id":playback.active_response_id.load(Ordering::SeqCst)
                }))
            })
            .await
            {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("diagnostic scenario exceeded 6s deadline")),
            };
        if let Some(mut c) = client {
            let _ = tokio::time::timeout(Duration::from_secs(1), c.stop()).await;
        }
        if tokio::time::timeout(Duration::from_secs(1), &mut server)
            .await
            .is_err()
        {
            server.abort();
            let _ = server.await;
        }
        playback.interrupt();
        drop(rx_model);
        drop(tx_user);
        drop(playback);
        let trace =
            execution.expect("bounded diagnostic scenario must run before acceptance assertion");
        eprintln!("AUDIT_LOCAL_BARGE_TRACE={trace}");
        assert_eq!(trace["next_pcm_accepted_by_mock_worker"], true,
            "after local cancel and cloud interruption, next authorized model PCM must reach playback; trace={trace}");
    }
}
