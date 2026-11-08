//! Audio generation, volume ramping, and WAV playback for alarms.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_WAV_BYTES: usize = 2 * 1024 * 1024;
const MAX_MELODY_SECONDS: usize = 30;

/// Generate pleasant synthetic 4-note chime sequence (E5, G#5, B5, E6) at specified sample rate.
/// Returns 16-bit signed PCM mono samples.
pub fn generate_synthetic_chime(sample_rate: u32) -> Vec<i16> {
    // Musical notes: E5 (659.25 Hz), G#5 (830.61 Hz), B5 (987.77 Hz), E6 (1318.51 Hz)
    let note_freqs = [659.25f32, 830.61, 987.77, 1318.51];
    let note_samples = (sample_rate as f32 * 0.22) as usize; // 220ms per note
    let gap_samples = (sample_rate as f32 * 0.03) as usize; // 30ms gap between notes
    let pause_samples = (sample_rate as f32 * 0.90) as usize; // 900ms silence after sequence

    let mut pcm =
        Vec::with_capacity((note_samples + gap_samples) * note_freqs.len() + pause_samples);

    for &freq in &note_freqs {
        for i in 0..note_samples {
            let t = i as f32 / sample_rate as f32;
            // 8ms linear attack to eliminate clicks, smooth exponential decay
            let attack = (t / 0.008).min(1.0);
            let decay = (-t * 6.0).exp();
            let env = attack * decay;

            // Fundamental + warm second harmonic
            let wave = (2.0 * std::f32::consts::PI * freq * t).sin()
                + 0.25 * (4.0 * std::f32::consts::PI * freq * t).sin();

            let sample = (wave * env * 14000.0).clamp(-32767.0, 32767.0) as i16;
            pcm.push(sample);
        }
        pcm.resize(pcm.len() + gap_samples, 0i16);
    }

    pcm.resize(pcm.len() + pause_samples, 0i16);
    pcm
}

/// Apply progressive volume ramp (fade-in) to PCM samples.
/// `progress` smoothly increases from `min_gain` to `max_gain` over `ramp_secs`.
pub fn apply_volume_ramp(
    samples: &[i16],
    elapsed_secs: f32,
    ramp_secs: f32,
    min_gain: f32,
    max_gain: f32,
) -> Vec<i16> {
    let progress = if ramp_secs > 0.0 {
        (elapsed_secs / ramp_secs).clamp(0.0, 1.0)
    } else {
        1.0
    };

    // Smooth cubic Hermite interpolation (ease-in-out)
    let factor = progress * progress * (3.0 - 2.0 * progress);
    let current_gain = min_gain + (max_gain - min_gain) * factor;

    samples
        .iter()
        .map(|&s| {
            let scaled = s as f32 * current_gain;
            scaled.clamp(-32767.0, 32767.0) as i16
        })
        .collect()
}

/// Load mono or stereo PCM16 WAV at 16, 24 or 48 kHz as 24 kHz mono.
/// Files above 2 MiB, clips longer than 30 seconds and invalid PCM are rejected.
pub fn load_wav_pcm_16(path: &Path) -> Result<Vec<i16>> {
    // Reject special files before opening, and bound reading if a regular file
    // grows between the size check and the read.
    let metadata = path.metadata().context("Alarm WAV unavailable")?;
    if !metadata.is_file() || metadata.len() > MAX_WAV_BYTES as u64 {
        bail!("Alarm WAV must be a regular file up to 2 MiB");
    }
    let file = File::open(path).context("Could not open alarm WAV")?;
    if !file.metadata()?.is_file() {
        bail!("Alarm WAV must be a regular file");
    }
    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_WAV_BYTES as u64 + 1).read_to_end(&mut data)?;
    if data.len() > MAX_WAV_BYTES {
        bail!("Alarm WAV exceeds 2 MiB");
    }
    decode_wav(&data)
}

fn decode_wav(data: &[u8]) -> Result<Vec<i16>> {
    let header = data.get(..12).context("WAV header is truncated")?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        bail!("Invalid WAV: missing RIFF/WAVE header");
    }

    let riff_size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let riff_end = riff_size.checked_add(8).context("WAV size overflow")?;
    let mut chunks = data.get(12..riff_end).context("Invalid RIFF size")?;
    let mut format = None;
    while !chunks.is_empty() {
        let header = chunks.get(..8).context("WAV chunk header is truncated")?;
        let chunk_size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        let payload = &chunks[8..];
        let chunk = payload.get(..chunk_size).context("Truncated WAV chunk")?;
        chunks = payload
            .get(chunk_size + chunk_size % 2..)
            .context("Missing WAV chunk padding")?;
        match &header[..4] {
            b"fmt " => format = Some(decode_format(chunk)?),
            b"data" => {
                let (channels, sample_rate) = format.context("Missing WAV format before data")?;
                return decode_samples(chunk, channels, sample_rate);
            }
            _ => {}
        }
    }
    bail!("Missing WAV data chunk")
}

fn decode_format(chunk: &[u8]) -> Result<(usize, usize)> {
    let format = chunk.get(..16).context("WAV format is truncated")?;
    let audio_format = u16::from_le_bytes(format[..2].try_into().unwrap());
    let channels = u16::from_le_bytes(format[2..4].try_into().unwrap()) as usize;
    let sample_rate = u32::from_le_bytes(format[4..8].try_into().unwrap()) as usize;
    let byte_rate = u32::from_le_bytes(format[8..12].try_into().unwrap()) as usize;
    let block_align = u16::from_le_bytes(format[12..14].try_into().unwrap()) as usize;
    let bit_depth = u16::from_le_bytes(format[14..].try_into().unwrap());
    if audio_format != 1
        || bit_depth != 16
        || !matches!(channels, 1 | 2)
        || !matches!(sample_rate, 16_000 | 24_000 | 48_000)
        || block_align != channels * 2
        || byte_rate != sample_rate * block_align
    {
        bail!("Alarm WAV requires mono or stereo PCM16 at 16, 24 or 48 kHz");
    }
    Ok((channels, sample_rate))
}

fn decode_samples(raw: &[u8], channels: usize, sample_rate: usize) -> Result<Vec<i16>> {
    let frame_size = channels * 2;
    let frames = raw.chunks_exact(frame_size);
    if frames.len() == 0 || !frames.remainder().is_empty() {
        bail!("WAV data must contain complete PCM frames");
    }
    if frames.len() > sample_rate * MAX_MELODY_SECONDS {
        bail!("Alarm WAV exceeds 30 seconds");
    }
    let mono: Vec<i16> = frames
        .map(|frame| {
            let left = i16::from_le_bytes([frame[0], frame[1]]);
            if channels == 1 {
                left
            } else {
                let right = i16::from_le_bytes([frame[2], frame[3]]);
                ((i32::from(left) + i32::from(right)) / 2) as i16
            }
        })
        .collect();
    let output = match sample_rate {
        24_000 => mono,
        48_000 => mono.into_iter().step_by(2).collect(),
        // Advance 2/3 of an input frame per output frame for 16 → 24 kHz.
        _ => (0..mono.len() * 3 / 2)
            .map(|out| {
                let position = out * 2;
                let index = position / 3;
                let fraction = (position % 3) as i32;
                let left = i32::from(mono[index]);
                let right = i32::from(mono.get(index + 1).copied().unwrap_or(mono[index]));
                ((left * (3 - fraction) + right * fraction) / 3) as i16
            })
            .collect(),
    };
    if output.iter().all(|&sample| sample == 0) {
        bail!("Alarm WAV contains no audible samples");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_synthetic_chime() {
        let pcm = generate_synthetic_chime(24000);
        // Expect around 1.9s of audio = ~45600 samples
        assert!(pcm.len() > 40000 && pcm.len() < 50000);

        // Verify non-zero peak
        let max_amp = pcm.iter().map(|&s| s.abs()).max().unwrap_or(0);
        assert!(
            max_amp > 5000,
            "Synthetic chime should have audible amplitude"
        );

        // Verify bounds
        for &s in &pcm {
            assert!(s >= -32767);
        }
    }

    #[test]
    fn test_apply_volume_ramp() {
        let input = vec![1000i16; 100];

        // At t = 0, gain is min_gain (0.15)
        let ramped_0 = apply_volume_ramp(&input, 0.0, 20.0, 0.15, 0.85);
        assert_eq!(ramped_0[0], 150);

        // At t = 20, gain is max_gain (0.85)
        let ramped_20 = apply_volume_ramp(&input, 20.0, 20.0, 0.15, 0.85);
        assert_eq!(ramped_20[0], 850);

        // At t = 10 (halfway), gain is around 0.50
        let ramped_10 = apply_volume_ramp(&input, 10.0, 20.0, 0.15, 0.85);
        assert!(ramped_10[0] > 400 && ramped_10[0] < 600);
    }

    #[test]
    fn test_load_wav_pcm_16_invalid_file() {
        let path = Path::new("/non/existent/path/melody.wav");
        assert!(load_wav_pcm_16(path).is_err());
    }

    fn mono_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let pcm_bytes = (samples.len() * 2) as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + pcm_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&pcm_bytes.to_le_bytes());
        wav.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
        wav
    }

    #[test]
    fn test_load_wav_pcm_16_in_memory_valid() {
        let wav = mono_wav(24_000, &[100, -200, 300, -400]);
        let temp_dir = std::env::temp_dir().join(format!("wav-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let file_path = temp_dir.join("test.wav");
        std::fs::write(&file_path, wav).unwrap();

        let samples = load_wav_pcm_16(&file_path).unwrap();
        assert_eq!(samples, vec![100, -200, 300, -400]);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn resample_16khz_with_odd_padded_metadata() {
        let mut wav = mono_wav(16_000, &[0, 300, 600, 900]);
        wav.splice(12..12, *b"JUNK\x01\0\0\0x\0");
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
        assert_eq!(decode_wav(&wav).unwrap(), [0, 200, 400, 600, 800, 900]);
    }

    #[test]
    fn reject_truncated_empty_or_silent_pcm() {
        let mut wav = mono_wav(24_000, &[100, 200]);
        wav.truncate(wav.len() - 2);
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
        assert!(decode_wav(&wav).is_err());
        assert!(decode_wav(&mono_wav(24_000, &[])).is_err());
        assert!(decode_wav(&mono_wav(24_000, &[0, 0])).is_err());
    }

    #[test]
    fn reject_wav_above_resource_limits() {
        let path = std::env::temp_dir().join(format!("wav-test-{}", uuid::Uuid::new_v4()));
        let file = File::create(&path).unwrap();
        file.set_len(MAX_WAV_BYTES as u64 + 1).unwrap();
        assert!(load_wav_pcm_16(&path).is_err());
        std::fs::remove_file(path).unwrap();

        let oversized_pcm = vec![0; 2 * (16_000 * MAX_MELODY_SECONDS + 1)];
        assert!(decode_samples(&oversized_pcm, 1, 16_000).is_err());
    }
}
