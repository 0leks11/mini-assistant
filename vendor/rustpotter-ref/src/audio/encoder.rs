use rubato::{FftFixedInOut, Resampler};

use crate::{config::AudioFmt, Endianness, Sample, SampleFormat};

/// Encode and convert audio to the supported format.
pub struct AudioEncoder {
    resampler: Option<FftFixedInOut<f32>>,
    resampler_input_buffer: Option<Vec<Vec<f32>>>,
    resampler_out_buffer: Option<Vec<Vec<f32>>>,
    source_sample_format: SampleFormat,
    source_channels: u16,
    source_endianness: Endianness,
    input_samples_per_frame: usize,
    output_samples_per_frame: usize,
}
impl AudioEncoder {
    pub fn get_input_frame_length(&self) -> usize {
        self.input_samples_per_frame
    }
    pub fn get_output_frame_length(&self) -> usize {
        self.output_samples_per_frame
    }
    pub fn get_input_byte_length(&self) -> usize {
        self.get_input_frame_length() * self.source_sample_format.get_bytes_per_sample() as usize
    }
    pub fn encode_and_resample(&mut self, buffer: &[u8]) -> Vec<f32> {
        let float_samples = match &self.source_sample_format {
            SampleFormat::I8 => encode_audio_bytes::<i8>(buffer, &self.source_endianness),
            SampleFormat::I16 => encode_audio_bytes::<i16>(buffer, &self.source_endianness),
            SampleFormat::I32 => encode_audio_bytes::<i32>(buffer, &self.source_endianness),
            SampleFormat::F32 => encode_audio_bytes::<f32>(buffer, &self.source_endianness),
        };
        self.reencode_to_mono_with_sample_rate(float_samples)
    }
    pub fn rencode_and_resample<T: Sample>(&mut self, buffer: Vec<T>) -> Vec<f32> {
        self.reencode_to_mono_with_sample_rate(
            buffer.into_iter().map(T::into_f32).collect::<Vec<f32>>(),
        )
    }
    fn reencode_to_mono_with_sample_rate(&mut self, buffer: Vec<f32>) -> Vec<f32> {
        let mono_buffer = if self.source_channels != 1 {
            buffer
                .chunks_exact(self.source_channels as usize)
                .map(|chunk| chunk[0])
                .collect::<Vec<f32>>()
        } else {
            buffer
        };
        if self.resampler.is_none() {
            mono_buffer
        } else {
            let waves_in = self.resampler_input_buffer.as_mut().unwrap();
            waves_in[0] = mono_buffer;
            let waves_out = self.resampler_out_buffer.as_mut().unwrap();
            self.resampler
                .as_mut()
                .unwrap()
                .process_into_buffer(waves_in, waves_out, None)
                .unwrap();
            waves_out.get(0).unwrap().to_vec()
        }
    }
    pub fn new(
        input_spec: &AudioFmt,
        frame_length_ms: usize,
        target_sample_rate: usize,
    ) -> Result<AudioEncoder, &'static str> {
        let mut input_samples_per_frame =
            (input_spec.sample_rate * frame_length_ms / 1000) * input_spec.channels as usize;
        let output_samples_per_frame = target_sample_rate * frame_length_ms / 1000;
        let resampler = if input_spec.sample_rate != target_sample_rate {
            let resampler = FftFixedInOut::<f32>::new(
                input_spec.sample_rate,
                target_sample_rate,
                output_samples_per_frame,
                1,
            )
            .map_err(|_| "Unsupported sample rate, unable to initialize the resampler")?;
            input_samples_per_frame = resampler.input_frames_next() * input_spec.channels as usize;
            Some(resampler)
        } else {
            None
        };
        Ok(AudioEncoder {
            input_samples_per_frame,
            output_samples_per_frame,
            resampler_out_buffer: if resampler.is_some() {
                Some(resampler.as_ref().unwrap().output_buffer_allocate(true))
            } else {
                None
            },
            resampler_input_buffer: if resampler.is_some() {
                Some(resampler.as_ref().unwrap().input_buffer_allocate(false))
            } else {
                None
            },
            resampler,
            source_sample_format: input_spec.sample_format.clone(),
            source_channels: input_spec.channels,
            source_endianness: input_spec.endianness.clone(),
        })
    }
}

fn encode_audio_bytes<T: Sample>(audio_buffer: &[u8], endianness: &Endianness) -> Vec<f32> {
    let buffer_chunks = audio_buffer.chunks_exact(T::get_byte_size());
    match endianness {
        Endianness::Little => buffer_chunks.map(T::from_le_bytes).collect::<Vec<T>>(),
        Endianness::Big => buffer_chunks.map(T::from_be_bytes).collect::<Vec<T>>(),
        Endianness::Native => buffer_chunks.map(T::from_ne_bytes).collect::<Vec<T>>(),
    }
    .into_iter()
    .map(T::into_f32)
    .collect()
}

#[test]
fn mono_pcm16_geometry_and_endianness() {
    let config = AudioFmt {
        sample_rate: 16_000,
        sample_format: SampleFormat::I16,
        channels: 1,
        endianness: Endianness::Little,
    };
    let mut encoder = AudioEncoder::new(&config, 30, 16_000).unwrap();
    assert_eq!(encoder.get_input_frame_length(), 480);
    assert_eq!(encoder.get_input_byte_length(), 960);
    assert_eq!(encoder.get_output_frame_length(), 480);
    let pcm: Vec<u8> = (0..480).flat_map(|_| 1000_i16.to_le_bytes()).collect();
    let output = encoder.encode_and_resample(&pcm);
    assert_eq!(output.len(), 480);
    assert!(output.iter().all(|x| (*x - 1000.0 / 32767.0).abs() < 1e-5));
}

#[test]
fn stereo_encoder_selects_first_channel_without_mixing_second() {
    let config = AudioFmt {
        sample_rate: 16_000,
        sample_format: SampleFormat::I16,
        channels: 2,
        endianness: Endianness::Little,
    };
    let mut encoder = AudioEncoder::new(&config, 30, 16_000).unwrap();
    assert_eq!(encoder.get_input_frame_length(), 960);
    let samples: Vec<i16> = (0..480).flat_map(|_| [1000_i16, -1000_i16]).collect();
    let output = encoder.rencode_and_resample(samples);
    assert_eq!(output.len(), 480);
    assert!(output.iter().all(|x| (*x - 1000.0 / 32767.0).abs() < 1e-5));
}
