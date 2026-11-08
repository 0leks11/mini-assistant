//! Offline personal MFCC/DTW enrollment. Never opens a microphone or a network.
use anyhow::{bail, ensure, Context, Result};
use rustpotter::{WakewordLoad, WakewordRef, WakewordRefBuildFromBuffers, WakewordSave};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 16_000;
const MAX_ENROLLMENT_BYTES: usize = 128 * 1024;
const MAX_REFERENCE_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_WAV_BYTES: usize = SAMPLE_RATE * 60 * 2 + 65_536;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentManifest {
    samples: Vec<AnnotatedSegment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnotatedSegment {
    path: PathBuf,
    start_ms: u64,
    /// Exclusive end in the original WAV timeline. Boundaries must contain the whole keyword.
    end_ms: u64,
}

fn read_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    ensure!(path.is_file(), "Input must be a regular file");
    let file = File::open(path).context("Input file unavailable")?;
    ensure!(file.metadata()?.is_file(), "Input must be a regular file");
    let mut data = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut data)?;
    ensure!(
        !data.is_empty() && data.len() <= max_bytes,
        "Input size outside limit"
    );
    Ok(data)
}

fn decode_pcm(bytes: &[u8], min_samples: usize, max_samples: usize) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::new(Cursor::new(bytes)).context("Invalid WAV")?;
    let spec = reader.spec();
    ensure!(
        spec.channels == 1
            && spec.sample_rate == 16_000
            && spec.bits_per_sample == 16
            && spec.sample_format == hound::SampleFormat::Int,
        "Expected mono 16kHz PCM16 WAV"
    );
    ensure!(
        (min_samples..=max_samples).contains(&(reader.len() as usize)),
        "WAV duration outside limit"
    );
    let mut samples = Vec::with_capacity(reader.len() as usize);
    for sample in reader.samples::<i16>() {
        ensure!(samples.len() < max_samples, "Too many PCM samples");
        samples.push(sample.context("Malformed PCM data")?);
    }
    ensure!(
        (min_samples..=max_samples).contains(&samples.len()),
        "WAV duration outside limit"
    );
    Ok(samples)
}

fn encode_pcm(samples: &[i16]) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(
            &mut cursor,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        for sample in samples {
            writer.write_sample(*sample)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

/// Trim obvious acquisition silence; this is enrollment preprocessing, not wake detection.
fn trim_enrollment(samples: &[i16]) -> Result<&[i16]> {
    let rms: Vec<f64> = samples
        .chunks(160)
        .map(|frame| {
            (frame.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / frame.len() as f64).sqrt()
        })
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(f64::total_cmp);
    let noise = sorted[sorted.len() / 10];
    let peak = *sorted.last().context("Empty recording")?;
    ensure!(
        peak >= 1.0 && peak >= noise * 1.8,
        "Recording lacks distinct keyword activity"
    );
    // Keep ordinary room noise out of the reference. A threshold capped at
    // 25% of a quiet keyword previously retained nearly the whole noise bed.
    let threshold = (peak * 0.30).max(noise * 3.0).min(peak * 0.65);
    // Select the contiguous activity around the strongest frame, rather than
    // joining unrelated room-noise peaks across the entire acquisition window.
    let peak_index = rms
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .context("Empty recording")?;
    let mut first = peak_index;
    for i in (0..peak_index).rev() {
        if rms[i] >= threshold {
            first = i;
        }
        if first - i >= 10 {
            break;
        }
    }
    let mut last = peak_index;
    for (i, value) in rms.iter().enumerate().skip(peak_index + 1) {
        if *value >= threshold {
            last = i;
        }
        if i - last >= 10 {
            break;
        }
    }
    let start = first.saturating_sub(8) * 160;
    let end = ((last + 1 + 12) * 160).min(samples.len());
    let result = &samples[start..end];
    ensure!(
        result.len() >= 4800,
        "Trimmed keyword is too short; record at least 0.3 seconds"
    );
    Ok(result)
}

fn reference_summary(reference: &WakewordRef) -> Value {
    json!({
        "kind": "personal_mfcc_dtw_reference",
        "templates": reference.samples_features.len(),
        "mfcc_dimension": reference.mfcc_size,
        "min_feature_frames": reference.samples_features.values().map(Vec::len).min().unwrap_or(0),
        "max_feature_frames": reference.samples_features.values().map(Vec::len).max().unwrap_or(0),
        "acoustic_quality_verified": false
    })
}

fn build_reference(output: &Path, paths: &[PathBuf]) -> Result<Value> {
    ensure!(
        (3..=8).contains(&paths.len()),
        "Provide 3 to 8 independent keyword WAV recordings"
    );
    let mut buffers = HashMap::new();
    let mut recorded = Vec::<Vec<i16>>::new();
    for (index, path) in paths.iter().enumerate() {
        let bytes = read_bounded(path, MAX_ENROLLMENT_BYTES)?;
        let samples = decode_pcm(&bytes, 4800, 48000)?;
        ensure!(
            !recorded.contains(&samples),
            "Duplicate recording; use independent samples"
        );
        let trimmed = trim_enrollment(&samples)?;
        buffers.insert(format!("sample-{index:02}"), encode_pcm(trimmed)?);
        recorded.push(samples);
    }
    save_reference(output, buffers)
}

/// Read explicitly annotated segments without energy trimming or normalization.
/// A longer acquisition file can contain multiple independently spoken keywords.
fn load_annotated_segments(manifest_path: &Path) -> Result<Vec<Vec<i16>>> {
    let bytes = read_bounded(manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: SegmentManifest =
        serde_json::from_slice(&bytes).context("Invalid segment manifest")?;
    ensure!(
        (3..=8).contains(&manifest.samples.len()),
        "Provide 3 to 8 independent keyword segments"
    );
    let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut segments = Vec::with_capacity(manifest.samples.len());
    for entry in manifest.samples {
        ensure!(
            !entry.path.as_os_str().is_empty() && entry.path.is_relative(),
            "Segment path must be nonempty and relative to the manifest"
        );
        ensure!(
            entry.start_ms < entry.end_ms
                && entry.end_ms <= 60_000
                && (300..=3000).contains(&(entry.end_ms - entry.start_ms)),
            "Segment duration must be 0.3 to 3 seconds within a source of at most 60 seconds"
        );
        // Milliseconds are exact multiples of 16 samples at the required 16kHz rate.
        let start = usize::try_from(
            entry
                .start_ms
                .checked_mul(16)
                .context("Segment offset overflow")?,
        )?;
        let end = usize::try_from(
            entry
                .end_ms
                .checked_mul(16)
                .context("Segment offset overflow")?,
        )?;
        let source = read_bounded(&base.join(entry.path), MAX_SOURCE_WAV_BYTES)?;
        let samples = decode_pcm(&source, 1, SAMPLE_RATE * 60)?;
        let segment = samples
            .get(start..end)
            .context("Segment extends beyond WAV data")?;
        ensure!(
            !segments.iter().any(|previous| previous == segment),
            "Duplicate segment; use independent recordings"
        );
        segments.push(segment.to_vec());
    }
    Ok(segments)
}

fn build_segment_reference(output: &Path, manifest_path: &Path) -> Result<Value> {
    let segments = load_annotated_segments(manifest_path)?;
    let buffers = segments
        .iter()
        .enumerate()
        .map(|(index, samples)| Ok((format!("sample-{index:02}"), encode_pcm(samples)?)))
        .collect::<Result<HashMap<_, _>>>()?;
    save_reference(output, buffers)
}

/// Both enrollment paths use the same bounded reference format and exclusive output creation.
fn save_reference(output: &Path, buffers: HashMap<String, Vec<u8>>) -> Result<Value> {
    let reference = WakewordRef::new_from_sample_buffers(
        "shtuchka".into(),
        Some(0.65),
        Some(0.60),
        buffers,
        16,
    )
    .map_err(anyhow::Error::msg)?;
    reference
        .validate_personal_reference()
        .map_err(anyhow::Error::msg)?;
    let bytes = reference.save_to_buffer().map_err(anyhow::Error::msg)?;
    ensure!(
        bytes.len() <= MAX_REFERENCE_BYTES,
        "Reference exceeds size limit"
    );
    // Round-trip the exact artifact through the same bounded loader used on Mini.
    WakewordRef::load_from_buffer(&bytes).map_err(anyhow::Error::msg)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(output)
        .context("Output exists or is not writable; existing reference was not replaced")?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(output);
        return Err(error.into());
    }
    let mut summary = reference_summary(&reference);
    summary["reference_bytes"] = json!(bytes.len());
    summary["status"] = json!("built_pending_acoustic_validation");
    Ok(summary)
}

fn evaluate(reference_path: &Path, audio: &Path) -> Result<Value> {
    let reference =
        WakewordRef::load_from_file(reference_path.to_str().context("Invalid model path")?)
            .map_err(anyhow::Error::msg)?;
    let summary = reference_summary(&reference);
    let mut detector =
        rustpotter::personal_reference_detector(reference).map_err(anyhow::Error::msg)?;
    let bytes = read_bounded(audio, MAX_SOURCE_WAV_BYTES)?;
    let samples = decode_pcm(&bytes, 1, SAMPLE_RATE * 60)?;
    let samples_per_frame = detector.get_samples_per_frame();
    ensure!(
        (1..=4096).contains(&samples_per_frame),
        "Invalid detector frame size"
    );
    let processing_started = std::time::Instant::now();
    let mut detection_offsets_ms = Vec::new();
    let mut synthetic_tail_detection_offsets_ms = Vec::new();
    let mut frames = 0usize;
    for chunk in samples.chunks(samples_per_frame) {
        let mut frame = chunk.to_vec();
        frame.resize(samples_per_frame, 0);
        frames += 1;
        if detector.process_samples(frame).is_some() {
            detection_offsets_ms
                .push(frames as f64 * samples_per_frame as f64 * 1000.0 / SAMPLE_RATE as f64);
        }
    }
    // Flush partial matching countdown just as subsequent idle PCM would do on Mini.
    let tail_frames = (SAMPLE_RATE * 2).div_ceil(samples_per_frame);
    for tail_index in 0..tail_frames {
        if detector
            .process_samples(vec![0_i16; samples_per_frame])
            .is_some()
        {
            synthetic_tail_detection_offsets_ms.push(
                (frames + tail_index + 1) as f64 * samples_per_frame as f64 * 1000.0
                    / SAMPLE_RATE as f64,
            );
        }
    }
    let processing_seconds = processing_started.elapsed().as_secs_f64();
    let processed_audio_seconds =
        (frames + tail_frames) as f64 * samples_per_frame as f64 / SAMPLE_RATE as f64;
    Ok(
        json!({"status":"evaluated", "reference":summary, "input_samples":samples.len(),
        "input_seconds":samples.len() as f64 / SAMPLE_RATE as f64,
        "input_frames":frames,"synthetic_tail_frames":tail_frames,
        "input_frame_padding_samples":frames * samples_per_frame - samples.len(),
        "detections":detection_offsets_ms.len() + synthetic_tail_detection_offsets_ms.len(),
        "detections_in_audio":detection_offsets_ms.len(),
        "detections_in_synthetic_tail":synthetic_tail_detection_offsets_ms.len(),
        "detection_offsets_ms":detection_offsets_ms,
        "synthetic_tail_detection_offsets_ms":synthetic_tail_detection_offsets_ms,
        "detection_offset_reference":"processed_frame_end_from_wav_start",
        "detection_offset_is_word_boundary":false,
        "processing_millis":processing_seconds * 1000.0,"processed_audio_seconds":processed_audio_seconds,
        "realtime_factor":processing_seconds / processed_audio_seconds}),
    )
}

fn run(args: &[String]) -> Result<Value> {
    match args {
        [mode, output, manifest] if mode == "build-segments" => {
            build_segment_reference(Path::new(output), Path::new(manifest))
        }
        [mode, output, samples @ ..] if mode == "build" => {
            let paths: Vec<_> = samples.iter().map(PathBuf::from).collect();
            build_reference(Path::new(output), &paths)
        }
        [mode, model] if mode == "validate" => {
            let reference = WakewordRef::load_from_file(model).map_err(anyhow::Error::msg)?;
            let mut summary = reference_summary(&reference);
            summary["status"] = json!("structurally_valid");
            Ok(summary)
        }
        [mode, model, audio] if mode == "test" => evaluate(Path::new(model), Path::new(audio)),
        _ => bail!("Usage: keyword-enroll build OUTPUT.rpw SAMPLE1.wav SAMPLE2.wav SAMPLE3.wav [up to 8]; build-segments OUTPUT.rpw MANIFEST.json; validate MODEL.rpw; test MODEL.rpw AUDIO.wav (max 60s)"),
    }
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!(
            "Offline personal MFCC/DTW enrollment (no microphone/network)\n\
            keyword-enroll build OUTPUT.rpw SAMPLE1.wav SAMPLE2.wav SAMPLE3.wav [up to 8]\n\
            keyword-enroll build-segments OUTPUT.rpw MANIFEST.json\n\
            Manifest: {{\"samples\":[{{\"path\":\"relative.wav\",\"start_ms\":100,\"end_ms\":1000}},...]}}\n\
            Segments are end-exclusive, 0.3-3s, not auto-trimmed; sources max 60s.\n\
            keyword-enroll validate MODEL.rpw\n\
            keyword-enroll test MODEL.rpw AUDIO.wav (mono 16kHz PCM16, max 60s)\n\
            Detection offsets identify processed frame ends, not word boundaries; synthetic tail is separate."
        );
        return;
    }
    match run(&args) {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("{}", json!({"status":"error","message":error.to_string()}));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "keyword-segments-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn annotated_fixture() -> (TestDirectory, PathBuf, Vec<Vec<i16>>, Value) {
        let dir = TestDirectory::new();
        std::fs::create_dir(dir.0.join("clips")).unwrap();
        std::fs::create_dir(dir.0.join("annotations")).unwrap();
        let mut sources = Vec::new();
        let mut entries = Vec::new();
        for variant in 0..3 {
            let mut samples = vec![0_i16; 32000];
            // Quiet consonant-like high frequencies preceding a much louder vowel.
            for (i, sample) in samples[1600..6400].iter_mut().enumerate() {
                *sample = ((i as f32 * 0.7 + variant as f32 * 0.1).sin() * 20.0) as i16;
            }
            for (i, sample) in samples[6400..16000].iter_mut().enumerate() {
                *sample = ((i as f32 * 0.13 + variant as f32 * 0.1).sin() * 3000.0) as i16;
            }
            let path = dir.0.join(format!("clips/source-{variant}.wav"));
            std::fs::write(&path, encode_pcm(&samples).unwrap()).unwrap();
            sources.push(samples);
            entries.push(json!({
                "path":format!("../clips/source-{variant}.wav"),
                "start_ms":100,"end_ms":1100
            }));
        }
        let manifest = json!({"samples":entries});
        let manifest_path = dir.0.join("annotations/segments.json");
        std::fs::write(&manifest_path, manifest.to_string()).unwrap();
        (dir, manifest_path, sources, manifest)
    }

    #[test]
    fn annotated_segments_preserve_quiet_onset_and_resolve_relative_to_manifest() {
        let (dir, manifest, sources, _) = annotated_fixture();
        let segments = load_annotated_segments(&manifest).unwrap();
        for (segment, source) in segments.iter().zip(sources) {
            assert_eq!(segment.as_slice(), &source[1600..17600]);
            assert!(segment[..3200].iter().any(|sample| *sample != 0));
            assert!(
                trim_enrollment(segment).unwrap().len() < segment.len(),
                "Energy trimming would remove the annotated quiet onset"
            );
        }
        let output = dir.0.join("personal.rpw");
        let result = run(&[
            "build-segments".into(),
            output.to_str().unwrap().into(),
            manifest.to_str().unwrap().into(),
        ])
        .unwrap();
        assert_eq!(result["templates"], 3);
        WakewordRef::load_from_file(output.to_str().unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let original = std::fs::read(&output).unwrap();
        assert!(build_segment_reference(&output, &manifest).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), original);
    }

    #[test]
    fn segment_manifest_rejects_unknown_fields_bad_types_and_sample_counts() {
        let (_dir, path, _, valid) = annotated_fixture();
        let mut unknown_root = valid.clone();
        unknown_root["comment"] = json!("not accepted");
        let mut unknown_entry = valid.clone();
        unknown_entry["samples"][0]["gain"] = json!(2);
        let mut invalid_start = valid.clone();
        invalid_start["samples"][0]["start_ms"] = json!(-1);
        let mut fractional_end = valid.clone();
        fractional_end["samples"][0]["end_ms"] = json!(1100.5);
        for invalid in [
            unknown_root,
            unknown_entry,
            invalid_start,
            fractional_end,
            json!({"samples":[]}),
            json!({"samples":valid["samples"].as_array().unwrap()[..2]}),
            json!({"samples":vec![valid["samples"][0].clone();9]}),
        ] {
            std::fs::write(&path, invalid.to_string()).unwrap();
            assert!(load_annotated_segments(&path).is_err());
        }
    }

    #[test]
    fn segment_ranges_paths_and_duplicate_pcm_fail_before_output_creation() {
        let (dir, path, _, valid) = annotated_fixture();
        let output = dir.0.join("must-not-exist.rpw");
        for (start, end) in [
            (100, 399),
            (0, 3001),
            (100, 100),
            (1000, 999),
            (59_500, 60_001),
            (100, 2100),
            (0, u64::MAX),
        ] {
            let mut invalid = valid.clone();
            invalid["samples"][0]["start_ms"] = json!(start);
            invalid["samples"][0]["end_ms"] = json!(end);
            std::fs::write(&path, invalid.to_string()).unwrap();
            assert!(build_segment_reference(&output, &path).is_err());
            assert!(!output.exists());
        }
        for bad_path in [
            PathBuf::new(),
            dir.0.join("clips/source-0.wav"),
            PathBuf::from("../clips/missing.wav"),
        ] {
            let mut invalid = valid.clone();
            invalid["samples"][0]["path"] = json!(bad_path);
            std::fs::write(&path, invalid.to_string()).unwrap();
            assert!(build_segment_reference(&output, &path).is_err());
            assert!(!output.exists());
        }
        let mut duplicate = valid.clone();
        duplicate["samples"][1] = duplicate["samples"][0].clone();
        std::fs::write(&path, duplicate.to_string()).unwrap();
        assert!(build_segment_reference(&output, &path).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn annotated_source_limits_and_exact_segment_boundaries() {
        let (dir, path, _, mut manifest) = annotated_fixture();
        let source = dir.0.join("clips/source-0.wav");
        std::fs::write(&source, encode_pcm(&vec![17; SAMPLE_RATE * 60]).unwrap()).unwrap();
        manifest["samples"][0]["start_ms"] = json!(57_000);
        manifest["samples"][0]["end_ms"] = json!(60_000);
        std::fs::write(&path, manifest.to_string()).unwrap();
        let segments = load_annotated_segments(&path).unwrap();
        assert_eq!(segments[0], vec![17; SAMPLE_RATE * 3]);
        manifest["samples"][0]["start_ms"] = json!(59_700);
        std::fs::write(&path, manifest.to_string()).unwrap();
        assert_eq!(load_annotated_segments(&path).unwrap()[0].len(), 4800);
        std::fs::write(&source, encode_pcm(&vec![17; SAMPLE_RATE * 61]).unwrap()).unwrap();
        assert!(load_annotated_segments(&path).is_err());
        std::fs::write(&source, vec![0_u8; MAX_SOURCE_WAV_BYTES + 1]).unwrap();
        assert!(load_annotated_segments(&path).is_err());
        std::fs::write(&path, vec![b' '; MAX_MANIFEST_BYTES + 1]).unwrap();
        assert!(load_annotated_segments(&path).is_err());
    }

    #[test]
    fn invalid_input_is_rejected_without_model_or_cloud_fallback() {
        assert!(decode_pcm(b"not wav", 1, 48000).is_err());
        assert!(trim_enrollment(&vec![0; 16000]).is_err());
        assert!(build_reference(Path::new("unused.rpw"), &[]).is_err());
    }

    #[test]
    fn truncated_pcm_and_wrong_rate_are_rejected() {
        let mut wav = encode_pcm(&vec![17; 16000]).unwrap();
        wav.truncate(wav.len() - 1);
        assert!(decode_pcm(&wav, 1, 48000).is_err());
        let mut wav = encode_pcm(&vec![17; 16000]).unwrap();
        wav[24..28].copy_from_slice(&8000_u32.to_le_bytes());
        assert!(decode_pcm(&wav, 1, 48000).is_err());
    }

    #[test]
    fn quiet_keyword_is_trimmed_without_memorizing_ambient_noise() {
        let mut samples: Vec<i16> = (0..48000)
            .map(|i| if i % 2 == 0 { 3 } else { -3 })
            .collect();
        // Mild ambient variation before/after the keyword must not widen it to 3s.
        for (i, sample) in samples.iter_mut().enumerate() {
            if !(8000..=40000).contains(&i) {
                *sample *= 2;
            }
        }
        for (i, sample) in samples[20000..26400].iter_mut().enumerate() {
            *sample = ((i as f32 * 0.13).sin() * 30.0) as i16;
        }
        let trimmed = trim_enrollment(&samples).unwrap();
        assert!((6400..14000).contains(&trimmed.len()));
    }

    #[test]
    fn enrollment_trim_preserves_signal_with_bounded_padding() {
        let mut samples = vec![0_i16; 48000];
        for (i, sample) in samples[16000..24000].iter_mut().enumerate() {
            *sample = ((i as f32 * 0.13).sin() * 500.0) as i16;
        }
        let trimmed = trim_enrollment(&samples).unwrap();
        assert!(trimmed.len() >= 8000);
        assert!(trimmed.len() < samples.len());
        assert!(trimmed.iter().any(|x| *x > 400));
    }

    #[test]
    fn offline_artifact_matches_held_out_synthetic_pattern_and_rejects_silence() {
        // A synthetic DSP contract, not evidence of recognizing a human word.
        let dir = std::env::temp_dir().join(format!(
            "keyword-enroll-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&dir).unwrap();
        let outcome = (|| -> Result<()> {
            let mut inputs = Vec::new();
            for variant in 0..4 {
                let mut samples = vec![0_i16; 48000];
                for (i, sample) in samples[12000..26400].iter_mut().enumerate() {
                    let t = i as f32 / 16000.0;
                    let envelope = (std::f32::consts::PI * t / 0.9).sin().powi(2);
                    let phase =
                        t * (200.0 + 180.0 * t) * std::f32::consts::TAU + variant as f32 * 0.12;
                    *sample =
                        ((phase.sin() + (phase * 2.7).sin() * 0.4) * envelope * 4000.0) as i16;
                }
                let path = dir.join(format!("sample-{variant}.wav"));
                std::fs::write(&path, encode_pcm(&samples)?)?;
                inputs.push(path);
            }
            let model = dir.join("personal.rpw");
            let result = build_reference(&model, &inputs[..3])?;
            assert_eq!(result["templates"], 3);
            assert!(
                build_reference(&model, &inputs[..3]).is_err(),
                "Must not overwrite a reference"
            );
            let evaluated = evaluate(&model, &inputs[3])?;
            assert!(evaluated["detections"].as_u64().unwrap() > 0);
            assert_eq!(evaluated["detection_offset_is_word_boundary"], false);
            assert_eq!(
                evaluated["detection_offset_reference"],
                "processed_frame_end_from_wav_start"
            );
            let check_offsets = |value: &Value| {
                let audio = value["detection_offsets_ms"].as_array().unwrap();
                let tail = value["synthetic_tail_detection_offsets_ms"]
                    .as_array()
                    .unwrap();
                assert_eq!(value["detections"], audio.len() + tail.len());
                assert_eq!(value["detections_in_audio"], audio.len());
                assert_eq!(value["detections_in_synthetic_tail"], tail.len());
                let padded_input_end_ms = (value["input_samples"].as_u64().unwrap()
                    + value["input_frame_padding_samples"].as_u64().unwrap())
                    as f64
                    / 16.0;
                for offsets in [audio, tail] {
                    assert!(offsets
                        .windows(2)
                        .all(|pair| { pair[0].as_f64().unwrap() < pair[1].as_f64().unwrap() }));
                }
                assert!(audio.iter().all(|offset| {
                    let ms = offset.as_f64().unwrap();
                    ms > 0.0 && ms <= padded_input_end_ms
                }));
                assert!(tail
                    .iter()
                    .all(|offset| { offset.as_f64().unwrap() > padded_input_end_ms }));
            };
            check_offsets(&evaluated);
            // End the WAV at the synthetic keyword's end. Non-eager detection
            // completes during the synthetic tail, not at an alleged word boundary.
            let original = decode_pcm(&std::fs::read(&inputs[3])?, 1, 48000)?;
            let short = dir.join("short.wav");
            std::fs::write(&short, encode_pcm(&original[..26400])?)?;
            let short_result = evaluate(&model, &short)?;
            assert!(
                short_result["detections_in_synthetic_tail"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            check_offsets(&short_result);
            let silence = dir.join("silence.wav");
            std::fs::write(&silence, encode_pcm(&vec![0; 48000])?)?;
            let silent_result = evaluate(&model, &silence)?;
            assert_eq!(silent_result["detections"], 0);
            assert_eq!(silent_result["detection_offsets_ms"], json!([]));
            assert_eq!(
                silent_result["synthetic_tail_detection_offsets_ms"],
                json!([])
            );
            Ok(())
        })();
        std::fs::remove_dir_all(dir).unwrap();
        outcome.unwrap();
    }
}
