//! Synthetic acoustic echo cancellation benchmark & unit tests for AecProcessor.
//!
//! Validates:
//! 1. Zero-distortion pass-through when disabled;
//! 2. Clean pass-through during silence;
//! 3. MDF filter convergence on synthetic room impulse response (RIR);
//! 4. Echo Return Loss Enhancement (ERLE >= 20 dB);
//! 5. Double-talk detection & near-end speech preservation.

use mini_assistant::audio::aec::{mdf::BLOCK_SIZE, AecConfig, AecProcessor};
use std::f32::consts::PI;

/// Simulates a synthetic Room Impulse Response (RIR) with direct path + exponential reverberation tail.
fn generate_synthetic_rir(
    direct_delay_samples: usize,
    tail_samples: usize,
    decay_rate: f32,
) -> Vec<f32> {
    let mut rir = vec![0.0; direct_delay_samples + tail_samples];
    // Direct path
    rir[direct_delay_samples] = 0.6;
    // Reverberation reflections
    for t in 1..tail_samples {
        let decay = (-decay_rate * (t as f32) / 100.0).exp();
        let reflection = 0.3 * decay * ((t as f32 * 0.1).sin() + 0.5 * (t as f32 * 0.23).cos());
        rir[direct_delay_samples + t] = reflection;
    }
    rir
}

/// Convolves signal with finite impulse response.
fn convolve(signal: &[f32], ir: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0; signal.len()];
    for n in 0..signal.len() {
        let mut sum = 0.0;
        for k in 0..ir.len() {
            if n >= k {
                sum += signal[n - k] * ir[k];
            }
        }
        out[n] = sum;
    }
    out
}

#[test]
fn test_aec_disabled_passthrough() {
    let config = AecConfig {
        enabled: false,
        ..Default::default()
    };
    let mut aec = AecProcessor::new(config);

    let mic = vec![1234i16; BLOCK_SIZE];
    let ref_audio = vec![5678i16; BLOCK_SIZE];
    let mut out = vec![0i16; BLOCK_SIZE];

    aec.process_frame(&mic, &ref_audio, &mut out);
    assert_eq!(out, mic);
}

#[test]
fn test_aec_silence_reference_passthrough() {
    let aec_cfg = AecConfig {
        enabled: true,
        ..Default::default()
    };
    let mut aec = AecProcessor::new(aec_cfg);

    let mic = vec![1000i16; BLOCK_SIZE];
    let silence = vec![0i16; BLOCK_SIZE];
    let mut out = vec![0i16; BLOCK_SIZE];

    aec.process_frame(&mic, &silence, &mut out);
    // When reference is silence, output must match mic input closely
    for i in 0..BLOCK_SIZE {
        assert_eq!(out[i], mic[i]);
    }
}

#[test]
fn test_aec_synthetic_rir_convergence_and_erle() {
    let delay_samples = 235; // Calibrated GPT Mini + KRK monitor bulk delay
    let config = AecConfig {
        enabled: true,
        bulk_delay_samples: delay_samples,
        step_size: 0.40,
        ..Default::default()
    };
    let mut aec = AecProcessor::new(config);

    // 1. Synthesize 3.0 seconds of multi-tone far-end music signal @ 16 kHz (190 blocks)
    let fs = 16000.0f32;
    let num_blocks = 190;
    let total_samples = num_blocks * BLOCK_SIZE;
    let mut far_end = Vec::with_capacity(total_samples);

    for n in 0..total_samples {
        let t = n as f32 / fs;
        // Pseudo-random wideband colored noise (excites all bins across 100-8000 Hz)
        let s = (0.3 * (2.0 * PI * 317.0 * t).sin()
            + 0.2 * (2.0 * PI * 643.0 * t).sin()
            + 0.2 * (2.0 * PI * 1151.0 * t).sin()
            + 0.15 * (2.0 * PI * 1987.0 * t).sin()
            + 0.15 * (2.0 * PI * 3121.0 * t).sin()
            + 0.1 * (2.0 * PI * 4897.0 * t).sin())
            * (1.0 + 0.2 * (2.0 * PI * 5.0 * t).sin()); // amplitude modulation
        far_end.push(s);
    }

    // 2. Synthesize room acoustic echo: d(n) = (x * h)(n)
    let rir = generate_synthetic_rir(delay_samples, 300, 0.6);
    let echo = convolve(&far_end, &rir);

    // 3. Convert to PCM16
    let far_end_pcm: Vec<i16> = far_end.iter().map(|&s| (s * 20000.0) as i16).collect();
    let mic_pcm: Vec<i16> = echo.iter().map(|&s| (s * 20000.0) as i16).collect();

    // 4. Feed frames through AEC and monitor ERLE
    let mut erle_history = Vec::new();
    let mut final_block_err_energy = 0.0f32;
    let mut final_block_mic_energy = 0.0f32;
    let mut out_frame = vec![0i16; BLOCK_SIZE];

    for b in 0..num_blocks {
        let start = b * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        let frame_mic = &mic_pcm[start..end];
        let frame_ref = &far_end_pcm[start..end];

        aec.process_frame(frame_mic, frame_ref, &mut out_frame);
        let metrics = aec.get_metrics();
        erle_history.push(metrics.erle_db);

        // Record last 10 blocks (settled convergence)
        if b >= num_blocks - 10 {
            for i in 0..BLOCK_SIZE {
                final_block_err_energy += (out_frame[i] as f32).powi(2);
                final_block_mic_energy += (frame_mic[i] as f32).powi(2);
            }
        }
    }

    let settled_erle = 10.0 * (final_block_mic_energy / final_block_err_energy.max(1.0)).log10();
    println!("AEC Synthetic Convergence Test:");
    println!("  Initial ERLE (Block 0..5): {:.1} dB", erle_history[2]);
    println!("  Mid ERLE (Block 30): {:.1} dB", erle_history[30]);
    println!("  Settled ERLE (Last 10 blocks): {:.1} dB", settled_erle);

    // Verify significant echo reduction (at least 20 dB ERLE on synthetic RIR)
    assert!(
        settled_erle >= 20.0,
        "AEC settled ERLE must be at least 20 dB, got {:.2} dB",
        settled_erle
    );
}

#[test]
fn test_aec_double_talk_protection() {
    let delay_samples = 235;
    let config = AecConfig {
        enabled: true,
        bulk_delay_samples: delay_samples,
        ..Default::default()
    };
    let mut aec = AecProcessor::new(config);

    // First train filter on 40 blocks of echo
    let fs = 16000.0f32;
    let num_blocks = 50;
    let total_samples = num_blocks * BLOCK_SIZE;

    let mut far_end = Vec::with_capacity(total_samples);
    for n in 0..total_samples {
        let t = n as f32 / fs;
        far_end.push(0.3 * (2.0 * PI * 440.0 * t).sin());
    }
    let rir = generate_synthetic_rir(delay_samples, 400, 0.5);
    let echo = convolve(&far_end, &rir);

    let far_end_pcm: Vec<i16> = far_end.iter().map(|&s| (s * 20000.0) as i16).collect();
    let mut mic_pcm: Vec<i16> = echo.iter().map(|&s| (s * 20000.0) as i16).collect();

    // Inject loud near-end speech burst in blocks 42..45 ("Штучка" uttered by user)
    for (i, sample) in mic_pcm
        .iter_mut()
        .enumerate()
        .take(45 * BLOCK_SIZE)
        .skip(42 * BLOCK_SIZE)
    {
        let t = i as f32 / fs;
        let near_speech = (0.7 * (2.0 * PI * 800.0 * t).sin() * 25000.0) as i16;
        *sample = sample.saturating_add(near_speech);
    }

    let mut dt_detected = false;
    let mut out_frame = vec![0i16; BLOCK_SIZE];

    for b in 0..num_blocks {
        let start = b * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        aec.process_frame(
            &mic_pcm[start..end],
            &far_end_pcm[start..end],
            &mut out_frame,
        );
        let metrics = aec.get_metrics();

        if (42..=44).contains(&b) {
            if metrics.double_talk_active {
                dt_detected = true;
            }
            // During near-end burst, output must contain the loud speech, not be zeroed out
            let rms_out = (out_frame.iter().map(|&s| (s as f32).powi(2)).sum::<f32>()
                / BLOCK_SIZE as f32)
                .sqrt();
            assert!(
                rms_out > 5000.0,
                "Near-end user voice must be preserved during double-talk"
            );
        }
    }

    assert!(
        dt_detected,
        "Double-talk must be detected during simultaneous user speech"
    );
}

#[test]
fn test_res_post_filter_enhancement() {
    let delay_samples = 235;
    let fs = 16000.0f32;
    let num_blocks = 80;
    let total_samples = num_blocks * BLOCK_SIZE;

    // Multi-tone signal
    let mut far_end = Vec::with_capacity(total_samples);
    for n in 0..total_samples {
        let t = n as f32 / fs;
        let s = (0.3 * (2.0 * PI * 317.0 * t).sin()
            + 0.2 * (2.0 * PI * 643.0 * t).sin()
            + 0.2 * (2.0 * PI * 1151.0 * t).sin()
            + 0.15 * (2.0 * PI * 1987.0 * t).sin()
            + 0.15 * (2.0 * PI * 3121.0 * t).sin())
            * (1.0 + 0.2 * (2.0 * PI * 5.0 * t).sin());
        far_end.push(s);
    }
    let rir = generate_synthetic_rir(delay_samples, 250, 0.5);
    let echo = convolve(&far_end, &rir);

    let far_end_pcm: Vec<i16> = far_end.iter().map(|&s| (s * 20000.0) as i16).collect();
    let mic_pcm: Vec<i16> = echo.iter().map(|&s| (s * 20000.0) as i16).collect();

    // 1. Run with RES disabled
    let config_no_res = AecConfig {
        enabled: true,
        bulk_delay_samples: delay_samples,
        step_size: 0.35,
        res_enabled: false,
        ..Default::default()
    };
    let mut aec_no_res = AecProcessor::new(config_no_res);
    let mut out_frame = vec![0i16; BLOCK_SIZE];
    for b in 0..num_blocks {
        let s = b * BLOCK_SIZE;
        let e = s + BLOCK_SIZE;
        aec_no_res.process_frame(&mic_pcm[s..e], &far_end_pcm[s..e], &mut out_frame);
    }
    let metrics_no_res = aec_no_res.get_metrics();

    // 2. Run with RES enabled
    let config_with_res = AecConfig {
        enabled: true,
        bulk_delay_samples: delay_samples,
        step_size: 0.35,
        res_enabled: true,
        res_floor_db: -20.0,
        ..Default::default()
    };
    let mut aec_with_res = AecProcessor::new(config_with_res);
    for b in 0..num_blocks {
        let s = b * BLOCK_SIZE;
        let e = s + BLOCK_SIZE;
        aec_with_res.process_frame(&mic_pcm[s..e], &far_end_pcm[s..e], &mut out_frame);
    }
    let metrics_with_res = aec_with_res.get_metrics();

    println!(
        "RES Comparison: No RES ERLE = {:.1} dB, With RES ERLE = {:.1} dB",
        metrics_no_res.erle_db, metrics_with_res.erle_db
    );

    // RES must provide substantial residual attenuation (higher ERLE)
    assert!(
        metrics_with_res.erle_db > metrics_no_res.erle_db,
        "RES Wiener filter must improve ERLE over pure linear MDF filter"
    );
    assert!(
        metrics_with_res.erle_db >= 25.0,
        "Target ERLE with RES enabled must reach >= 25 dB"
    );
}

#[test]
fn test_aec_realistic_low_coupling_double_talk() {
    // Reproduces real GPT Mini + KRK 60W coupling (Mic/Ref ~ 0.10) with 2m user speech
    let delay_samples = 16;
    let config = AecConfig {
        enabled: true,
        bulk_delay_samples: delay_samples,
        ..Default::default()
    };
    let mut aec = AecProcessor::new(config);

    let fs = 16000.0f32;
    let num_blocks = 120;
    let total_samples = num_blocks * BLOCK_SIZE;
    let mut far_end = Vec::with_capacity(total_samples);
    for n in 0..total_samples {
        let t = n as f32 / fs;
        let s = (0.35 * (2.0 * PI * 220.0 * t).sin()
            + 0.25 * (2.0 * PI * 587.0 * t).sin()
            + 0.20 * (2.0 * PI * 1320.0 * t).sin())
            * 0.3;
        far_end.push(s);
    }
    // Non-resonant decaying RIR modeling ~0.10 acoustic coupling ratio (no artificial 254Hz/585Hz gain > 1)
    let mut rir = vec![0.0f32; 280];
    rir[29] = -0.07;
    for t in 1..200 {
        let decay = (-0.8 * (t as f32) / 50.0).exp();
        rir[29 + t] = 0.015 * decay * ((t as f32 * 0.71).sin() + 0.5 * (t as f32 * 1.13).cos());
    }
    let echo = convolve(&far_end, &rir);

    let far_end_pcm: Vec<i16> = far_end.iter().map(|&s| (s * 25000.0) as i16).collect();
    let mut mic_pcm: Vec<i16> = echo.iter().map(|&s| (s * 25000.0) as i16).collect();

    // Inject realistic 2m near-end speech (RMS ~ 700, well below DAC ref RMS ~ 2500) at blocks 85..100
    for (i, sample) in mic_pcm
        .iter_mut()
        .enumerate()
        .take(100 * BLOCK_SIZE)
        .skip(85 * BLOCK_SIZE)
    {
        let t = i as f32 / fs;
        let near_speech = (1000.0 * (2.0 * PI * 680.0 * t).sin()) as i16;
        *sample = sample.saturating_add(near_speech);
    }

    let mut out_frame = vec![0i16; BLOCK_SIZE];
    let mut single_talk_dt = 0usize;
    let mut double_talk_detected = 0usize;
    let mut speech_out_sq = 0.0f64;

    for b in 0..num_blocks {
        let s = b * BLOCK_SIZE;
        let e = s + BLOCK_SIZE;
        aec.process_frame(&mic_pcm[s..e], &far_end_pcm[s..e], &mut out_frame);
        let dt = aec.get_metrics().double_talk_active;
        if (40..80).contains(&b) && dt {
            single_talk_dt += 1;
        }
        if (86..100).contains(&b) {
            if dt {
                double_talk_detected += 1;
            }
            speech_out_sq += out_frame
                .iter()
                .map(|&v| f64::from(v) * f64::from(v))
                .sum::<f64>();
        }
    }

    let preserved_rms = (speech_out_sq / (14 * BLOCK_SIZE) as f64).sqrt();
    assert_eq!(single_talk_dt, 0, "Pure echo must not falsely trigger DTD");
    assert!(
        double_talk_detected >= 10,
        "DTD must trigger at real Mic/Ref~0.10 coupling (detected {double_talk_detected}/14)"
    );
    assert!(
        preserved_rms > 600.0,
        "Near-end 2m voice must be preserved without RES attenuation (rms={preserved_rms:.1})"
    );
}
