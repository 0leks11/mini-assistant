//! Frozen offline M-AEC & KWS benchmark harness for GPT Mini Hill-Climb validation.
//!
//! Feeds real 8-channel S32_LE @ 48 kHz ALSA captures (`hw:0,2`) through `Decimator` + `AecProcessor`,
//! logs per-block metrics and summary JSON, writes Before/Ref/After 16 kHz mono WAVs for STFT
//! spectrogram analysis, and optionally profiles `KeywordEngine` latency/detections.

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavSpec, WavWriter};
use mini_assistant::audio::aec::{mdf::BLOCK_SIZE, AecConfig};
use mini_assistant::audio::alsa_capture::{ChannelMode, Decimator};
use mini_assistant::config::WakewordCalibrationConfig;
use mini_assistant::wakeword::keyword::KeywordEngine;
use mini_assistant::wakeword::WakeWordEngine;
use serde::Serialize;
use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const ALSA_PERIOD_BYTES: usize = 512 * 8 * 4; // 16384 bytes = 10.67 ms @ 48 kHz 8ch S32_LE
const SAMPLE_RATE_16K: u32 = 16_000;
const BLOCK_MS: u64 = 16;

struct Args {
    input: PathBuf,
    out_dir: PathBuf,
    channel: ChannelMode,
    eval_start_ms: u64,
    eval_end_ms: u64,
    steady_offset_ms: u64,
    aec: AecConfig,
    mic_gain: f32,
    kws_model: Option<PathBuf>,
    kws_threshold: f32,
    kws_avg_threshold: f32,
    kws_min_scores: usize,
    kws_band_pass: bool,
    kws_band_low_hz: f32,
    kws_band_high_hz: f32,
    kws_gain_norm: bool,
}

#[derive(Serialize)]
struct Summary {
    input_path: String,
    channel_mode: String,
    audio_duration_ms: u64,
    total_blocks: usize,
    eval_start_ms: u64,
    eval_end_ms: u64,
    steady_start_ms: u64,
    eval_blocks: usize,
    steady_blocks: usize,
    erle_db: f64,
    steady_erle_db: f64,
    mic_rms_dbfs: f64,
    ref_rms_dbfs: f64,
    err_rms_dbfs: f64,
    steady_mic_rms_dbfs: f64,
    steady_err_rms_dbfs: f64,
    double_talk_blocks: usize,
    aec_wall_ms: f64,
    aec_cpu_1core_pct: f64,
    aec_cpu_4core_pct: f64,
    aec_config: AecConfig,
    kws: Option<KwsSummary>,
}

#[derive(Serialize)]
struct KwsSummary {
    model_path: String,
    wall_ms_before: f64,
    wall_ms_after: f64,
    cpu_1core_pct_before: f64,
    cpu_1core_pct_after: f64,
    cpu_4core_pct_after: f64,
    detections_before: usize,
    detections_after: usize,
    max_score_before: f32,
    max_avg_score_before: f32,
    max_score_after: f32,
    max_avg_score_after: f32,
}

fn parse_bool(val: &str) -> Result<bool> {
    match val.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("Invalid boolean value: {val}"),
    }
}

fn parse_channel(val: &str) -> Result<(ChannelMode, String)> {
    match val.trim().to_ascii_lowercase().as_str() {
        "ch0" => Ok((ChannelMode::Ch0, "ch0".into())),
        "avg4" => Ok((ChannelMode::Avg4, "avg4".into())),
        "ch1" => Ok((ChannelMode::Ch(1), "ch1".into())),
        "ch2" => Ok((ChannelMode::Ch(2), "ch2".into())),
        "ch3" => Ok((ChannelMode::Ch(3), "ch3".into())),
        other => bail!("Unsupported channel mode: {other}"),
    }
}

fn parse_args() -> Result<Args> {
    let mut input: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut channel = ChannelMode::Ch0;
    let mut eval_start_ms = 0u64;
    let mut eval_end_ms = u64::MAX;
    let mut steady_offset_ms = 400u64;
    let mut aec = AecConfig {
        enabled: true,
        ..AecConfig::default()
    };
    let mut mic_gain = 1.0f32;
    let mut kws_model: Option<PathBuf> = None;
    let mut kws_threshold = 0.32f32;
    let mut kws_avg_threshold = 0.20f32;
    let mut kws_min_scores = 2usize;
    let mut kws_band_pass = true;
    let mut kws_band_low_hz = 200.0f32;
    let mut kws_band_high_hz = 4000.0f32;
    let mut kws_gain_norm = false;

    let mut it = env::args().skip(1);
    while let Some(flag) = it.next() {
        let next_val = |flag: &str, it: &mut std::iter::Skip<env::Args>| -> Result<String> {
            it.next()
                .with_context(|| format!("Missing value for {flag}"))
        };
        match flag.as_str() {
            "--input" => input = Some(PathBuf::from(next_val(&flag, &mut it)?)),
            "--out-dir" => out_dir = Some(PathBuf::from(next_val(&flag, &mut it)?)),
            "--channel" => {
                let (mode, _) = parse_channel(&next_val(&flag, &mut it)?)?;
                channel = mode;
            }
            "--eval-start-ms" => eval_start_ms = next_val(&flag, &mut it)?.parse()?,
            "--eval-end-ms" => eval_end_ms = next_val(&flag, &mut it)?.parse()?,
            "--steady-offset-ms" => steady_offset_ms = next_val(&flag, &mut it)?.parse()?,
            "--aec-enabled" => aec.enabled = parse_bool(&next_val(&flag, &mut it)?)?,
            "--bulk-delay" => aec.bulk_delay_samples = next_val(&flag, &mut it)?.parse()?,
            "--step-size" => aec.step_size = next_val(&flag, &mut it)?.parse()?,
            "--power-leak" => aec.power_leak = next_val(&flag, &mut it)?.parse()?,
            "--regularization" => aec.regularization = next_val(&flag, &mut it)?.parse()?,
            "--max-attenuation-db" => aec.max_attenuation_db = next_val(&flag, &mut it)?.parse()?,
            "--res-enabled" => aec.res_enabled = parse_bool(&next_val(&flag, &mut it)?)?,
            "--res-floor-db" => aec.res_floor_db = next_val(&flag, &mut it)?.parse()?,
            "--res-overdrive" => aec.res_overdrive = next_val(&flag, &mut it)?.parse()?,
            "--mic-gain" => mic_gain = next_val(&flag, &mut it)?.parse()?,
            "--config-json" => {
                let path = next_val(&flag, &mut it)?;
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read config JSON: {path}"))?;
                aec = serde_json::from_str(&raw)?;
            }
            "--kws-model" => kws_model = Some(PathBuf::from(next_val(&flag, &mut it)?)),
            "--kws-threshold" => kws_threshold = next_val(&flag, &mut it)?.parse()?,
            "--kws-avg-threshold" => kws_avg_threshold = next_val(&flag, &mut it)?.parse()?,
            "--kws-min-scores" => kws_min_scores = next_val(&flag, &mut it)?.parse()?,
            "--kws-band-pass" => kws_band_pass = parse_bool(&next_val(&flag, &mut it)?)?,
            "--kws-band-low-hz" => kws_band_low_hz = next_val(&flag, &mut it)?.parse()?,
            "--kws-band-high-hz" => kws_band_high_hz = next_val(&flag, &mut it)?.parse()?,
            "--kws-gain-norm" => kws_gain_norm = parse_bool(&next_val(&flag, &mut it)?)?,
            other => bail!("Unknown argument: {other}"),
        }
    }

    Ok(Args {
        input: input.context("--input is required")?,
        out_dir: out_dir.context("--out-dir is required")?,
        channel,
        eval_start_ms,
        eval_end_ms,
        steady_offset_ms,
        aec,
        mic_gain,
        kws_model,
        kws_threshold,
        kws_avg_threshold,
        kws_min_scores,
        kws_band_pass,
        kws_band_low_hz,
        kws_band_high_hz,
        kws_gain_norm,
    })
}

fn rms_dbfs(sum_sq: f64, samples: usize) -> f64 {
    if samples == 0 || sum_sq <= 0.0 {
        return -120.0;
    }
    let rms = (sum_sq / samples as f64).sqrt() / 32768.0;
    if rms > 1e-6 {
        20.0 * rms.log10()
    } else {
        -120.0
    }
}

fn chunk_energy_i16(bytes: &[u8]) -> f64 {
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs
        .iter()
        .map(|b| {
            let s = f64::from(i16::from_le_bytes(*b));
            s * s
        })
        .sum()
}

fn write_wav(path: &Path, chunks: &[Vec<u8>]) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE_16K,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec)?;
    for chunk in chunks {
        let (pairs, _) = chunk.as_chunks::<2>();
        for pair in pairs {
            writer.write_sample(i16::from_le_bytes(*pair))?;
        }
    }
    writer.finalize()?;
    Ok(())
}

fn run_kws_pass(
    model_path: &Path,
    cal: &WakewordCalibrationConfig,
    chunks: &[Vec<u8>],
) -> Result<(f64, usize, f32, f32)> {
    let mut engine = KeywordEngine::load_with_calibration(model_path, Some(cal))?;
    let mut detections = 0usize;
    let mut max_score = 0.0f32;
    let mut max_avg_score = 0.0f32;

    let t0 = Instant::now();
    for chunk in chunks {
        if engine.process(chunk) {
            detections += 1;
        }
        if let Some(obs) = engine.take_observation() {
            if obs.score.is_finite() {
                max_score = max_score.max(obs.score);
            }
            if obs.avg_score.is_finite() {
                max_avg_score = max_avg_score.max(obs.avg_score);
            }
        }
    }
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    Ok((wall_ms, detections, max_score, max_avg_score))
}

fn channel_label(mode: ChannelMode) -> String {
    match mode {
        ChannelMode::Ch0 => "ch0".into(),
        ChannelMode::Avg4 => "avg4".into(),
        ChannelMode::Ch(i) => format!("ch{i}"),
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    fs::create_dir_all(&args.out_dir)?;

    let raw_bytes = fs::read(&args.input)
        .with_context(|| format!("Failed to read raw capture {}", args.input.display()))?;

    let mut dec_before = Decimator::new(args.channel, 16, args.mic_gain);
    let mut dec_ref = Decimator::new(ChannelMode::Ch(6), 16, 1.0);
    let mut dec_aec = Decimator::new(args.channel, 16, args.mic_gain).with_aec(args.aec.clone());

    let mut before_chunks = Vec::new();
    let mut ref_chunks = Vec::new();
    for period in raw_bytes.chunks(ALSA_PERIOD_BYTES) {
        before_chunks.extend(dec_before.process(period));
        ref_chunks.extend(dec_ref.process(period));
    }

    let mut after_chunks = Vec::with_capacity(before_chunks.len());
    let mut block_metrics = Vec::with_capacity(before_chunks.len());

    let t_aec = Instant::now();
    for period in raw_bytes.chunks(ALSA_PERIOD_BYTES) {
        let produced = dec_aec.process(period);
        for chunk in produced {
            after_chunks.push(chunk);
            if let Some(m) = dec_aec.aec_metrics() {
                block_metrics.push(m);
            }
        }
    }
    let aec_wall_ms = t_aec.elapsed().as_secs_f64() * 1000.0;

    let total_blocks = before_chunks
        .len()
        .min(ref_chunks.len())
        .min(after_chunks.len());
    let audio_duration_ms = total_blocks as u64 * BLOCK_MS;
    let steady_start_ms = args.eval_start_ms.saturating_add(args.steady_offset_ms);

    let mut eval_mic_sq = 0.0f64;
    let mut eval_ref_sq = 0.0f64;
    let mut eval_err_sq = 0.0f64;
    let mut eval_samples = 0usize;
    let mut eval_blocks = 0usize;
    let mut steady_mic_sq = 0.0f64;
    let mut steady_err_sq = 0.0f64;
    let mut steady_samples = 0usize;
    let mut steady_blocks = 0usize;
    let mut dt_blocks = 0usize;

    let metrics_file = File::create(args.out_dir.join("metrics.jsonl"))?;
    let mut metrics_writer = BufWriter::new(metrics_file);

    for idx in 0..total_blocks {
        let t_ms = idx as u64 * BLOCK_MS;
        let mic_sq = chunk_energy_i16(&before_chunks[idx]);
        let ref_sq = chunk_energy_i16(&ref_chunks[idx]);
        let err_sq = chunk_energy_i16(&after_chunks[idx]);

        let inst_erle_db = if err_sq > 1.0 && mic_sq > 1.0 {
            10.0 * (mic_sq / err_sq).log10()
        } else {
            0.0
        };

        let m = block_metrics.get(idx);
        let filter_erle_db = m.map_or(0.0, |v| f64::from(v.erle_db));
        let dt_active = m.is_some_and(|v| v.double_talk_active);

        if t_ms >= args.eval_start_ms && t_ms < args.eval_end_ms {
            eval_mic_sq += mic_sq;
            eval_ref_sq += ref_sq;
            eval_err_sq += err_sq;
            eval_samples += BLOCK_SIZE;
            eval_blocks += 1;
            if dt_active {
                dt_blocks += 1;
            }
            if t_ms >= steady_start_ms {
                steady_mic_sq += mic_sq;
                steady_err_sq += err_sq;
                steady_samples += BLOCK_SIZE;
                steady_blocks += 1;
            }
        }

        writeln!(
            metrics_writer,
            "{}",
            serde_json::json!({
                "block": idx,
                "t_ms": t_ms,
                "mic_rms": (mic_sq / BLOCK_SIZE as f64).sqrt(),
                "ref_rms": (ref_sq / BLOCK_SIZE as f64).sqrt(),
                "err_rms": (err_sq / BLOCK_SIZE as f64).sqrt(),
                "inst_erle_db": inst_erle_db,
                "filter_erle_db": filter_erle_db,
                "double_talk": dt_active,
            })
        )?;
    }
    metrics_writer.flush()?;

    write_wav(
        &args.out_dir.join("mic_before_16k.wav"),
        &before_chunks[..total_blocks],
    )?;
    write_wav(
        &args.out_dir.join("ref_loopback_16k.wav"),
        &ref_chunks[..total_blocks],
    )?;
    write_wav(
        &args.out_dir.join("aec_after_16k.wav"),
        &after_chunks[..total_blocks],
    )?;

    let erle_db = if eval_err_sq > 1.0 && eval_mic_sq > 1.0 {
        10.0 * (eval_mic_sq / eval_err_sq).log10()
    } else {
        0.0
    };
    let steady_erle_db = if steady_err_sq > 1.0 && steady_mic_sq > 1.0 {
        10.0 * (steady_mic_sq / steady_err_sq).log10()
    } else {
        0.0
    };
    let dur_f64 = (audio_duration_ms.max(1)) as f64;
    let aec_cpu_1core_pct = 100.0 * aec_wall_ms / dur_f64;

    let kws = if let Some(ref model_path) = args.kws_model {
        let cal = WakewordCalibrationConfig {
            threshold: args.kws_threshold,
            avg_threshold: args.kws_avg_threshold,
            min_scores: args.kws_min_scores,
            score_mode: "max".into(),
            eager: true,
            band_pass: args.kws_band_pass,
            band_low_hz: args.kws_band_low_hz,
            band_high_hz: args.kws_band_high_hz,
            gain_normalizer: args.kws_gain_norm,
            gain_min: 0.1,
            gain_max: 1.0,
            ..Default::default()
        };

        let (wb, db, sb, ab) = run_kws_pass(model_path, &cal, &before_chunks[..total_blocks])?;
        let (wa, da, sa, aa) = run_kws_pass(model_path, &cal, &after_chunks[..total_blocks])?;
        Some(KwsSummary {
            model_path: model_path.display().to_string(),
            wall_ms_before: wb,
            wall_ms_after: wa,
            cpu_1core_pct_before: 100.0 * wb / dur_f64,
            cpu_1core_pct_after: 100.0 * wa / dur_f64,
            cpu_4core_pct_after: 25.0 * wa / dur_f64,
            detections_before: db,
            detections_after: da,
            max_score_before: sb,
            max_avg_score_before: ab,
            max_score_after: sa,
            max_avg_score_after: aa,
        })
    } else {
        None
    };

    let summary = Summary {
        input_path: args.input.display().to_string(),
        channel_mode: channel_label(args.channel),
        audio_duration_ms,
        total_blocks,
        eval_start_ms: args.eval_start_ms,
        eval_end_ms: args.eval_end_ms.min(audio_duration_ms),
        steady_start_ms,
        eval_blocks,
        steady_blocks,
        erle_db,
        steady_erle_db,
        mic_rms_dbfs: rms_dbfs(eval_mic_sq, eval_samples),
        ref_rms_dbfs: rms_dbfs(eval_ref_sq, eval_samples),
        err_rms_dbfs: rms_dbfs(eval_err_sq, eval_samples),
        steady_mic_rms_dbfs: rms_dbfs(steady_mic_sq, steady_samples),
        steady_err_rms_dbfs: rms_dbfs(steady_err_sq, steady_samples),
        double_talk_blocks: dt_blocks,
        aec_wall_ms,
        aec_cpu_1core_pct,
        aec_cpu_4core_pct: aec_cpu_1core_pct / 4.0,
        aec_config: args.aec,
        kws,
    };

    let json = serde_json::to_string_pretty(&summary)?;
    fs::write(args.out_dir.join("summary.json"), &json)?;
    println!("{json}");
    Ok(())
}
