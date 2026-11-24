//! Diagnostic test to observe adaptation and energy step by step
use mini_assistant::audio::aec::{mdf::BLOCK_SIZE, AecConfig, AecProcessor};
use std::f32::consts::PI;

fn main() {
    let config = AecConfig {
        enabled: true,
        bulk_delay_samples: 0, // zero delay for direct test
        step_size: 0.25,
        ..Default::default()
    };
    let mut aec = AecProcessor::new(config);

    // Pure 440 Hz tone
    let fs = 16000.0f32;
    let num_blocks = 40;
    let mut out = vec![0i16; BLOCK_SIZE];

    for b in 0..num_blocks {
        let mut mic = vec![0i16; BLOCK_SIZE];
        let mut ref_audio = vec![0i16; BLOCK_SIZE];

        for i in 0..BLOCK_SIZE {
            let t = (b * BLOCK_SIZE + i) as f32 / fs;
            let val = (0.5 * (2.0 * PI * 440.0 * t).sin() * 20000.0) as i16;
            ref_audio[i] = val;
            // Echo is simply 0.8 * reference
            mic[i] = (val as f32 * 0.8) as i16;
        }

        aec.process_frame(&mic, &ref_audio, &mut out);
        let m = aec.get_metrics();
        let mic_rms = (mic.iter().map(|&s| (s as f32).powi(2)).sum::<f32>() / 256.0).sqrt();
        let err_rms = (out.iter().map(|&s| (s as f32).powi(2)).sum::<f32>() / 256.0).sqrt();
        println!(
            "Block {:2}: Mic RMS = {:6.1}, Err RMS = {:6.1}, ERLE = {:5.1} dB, DT = {}",
            b, mic_rms, err_rms, m.erle_db, m.double_talk_active
        );
    }
}
