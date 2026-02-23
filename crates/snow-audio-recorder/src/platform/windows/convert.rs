use crate::error::{AudioError, AudioResult};
use crate::format::{AudioFormat, AudioSampleFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeSampleFormat {
    F32,
    I16,
    I32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeAudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: NativeSampleFormat,
}

impl NativeAudioFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self.sample_format {
            NativeSampleFormat::F32 => 4,
            NativeSampleFormat::I16 => 2,
            NativeSampleFormat::I32 => 4,
        }
    }

    pub fn bytes_per_frame(self) -> AudioResult<usize> {
        usize::from(self.channels)
            .checked_mul(self.bytes_per_sample())
            .ok_or(AudioError::BufferOverflow)
    }
}

pub(crate) struct AudioConverter {
    input: NativeAudioFormat,
    output: AudioFormat,
    decode_buffer: Vec<f32>,
    channel_buffer: Vec<f32>,
    resample_buffer: Vec<f32>,
    resampler: Option<LinearResampler>,
}

impl AudioConverter {
    pub fn new(input: NativeAudioFormat, output: AudioFormat) -> AudioResult<Self> {
        output.validate()?;

        let resampler = if input.sample_rate == output.sample_rate {
            None
        } else {
            Some(LinearResampler::new(
                input.sample_rate,
                output.sample_rate,
                output.channels,
            ))
        };

        Ok(Self {
            input,
            output,
            decode_buffer: Vec::new(),
            channel_buffer: Vec::new(),
            resample_buffer: Vec::new(),
            resampler,
        })
    }

    pub fn convert_chunk(&mut self, input_bytes: &[u8], input_frames: u32) -> AudioResult<Vec<u8>> {
        if input_frames == 0 {
            return Ok(Vec::new());
        }

        decode_interleaved_to_f32(
            input_bytes,
            input_frames,
            self.input.channels,
            self.input.sample_format,
            &mut self.decode_buffer,
        )?;

        convert_channels(
            &self.decode_buffer,
            self.input.channels,
            self.output.channels,
            &mut self.channel_buffer,
        );

        let output_samples: &[f32] = if let Some(resampler) = self.resampler.as_mut() {
            self.resample_buffer.clear();
            resampler.process(&self.channel_buffer, &mut self.resample_buffer);
            &self.resample_buffer
        } else {
            &self.channel_buffer
        };

        encode_from_f32(output_samples, self.output.sample_format)
    }
}

fn decode_interleaved_to_f32(
    input: &[u8],
    frames: u32,
    channels: u16,
    format: NativeSampleFormat,
    out: &mut Vec<f32>,
) -> AudioResult<()> {
    let channels_usize = usize::from(channels);
    let sample_count = (frames as usize)
        .checked_mul(channels_usize)
        .ok_or(AudioError::BufferOverflow)?;

    out.clear();
    out.reserve(sample_count);

    match format {
        NativeSampleFormat::F32 => {
            let needed = sample_count
                .checked_mul(4)
                .ok_or(AudioError::BufferOverflow)?;
            if input.len() < needed {
                return Err(AudioError::BufferOverflow);
            }
            for chunk in input[..needed].chunks_exact(4) {
                let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(sample);
            }
        }
        NativeSampleFormat::I16 => {
            let needed = sample_count
                .checked_mul(2)
                .ok_or(AudioError::BufferOverflow)?;
            if input.len() < needed {
                return Err(AudioError::BufferOverflow);
            }
            for chunk in input[..needed].chunks_exact(2) {
                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                out.push(sample as f32 / i16::MAX as f32);
            }
        }
        NativeSampleFormat::I32 => {
            let needed = sample_count
                .checked_mul(4)
                .ok_or(AudioError::BufferOverflow)?;
            if input.len() < needed {
                return Err(AudioError::BufferOverflow);
            }
            for chunk in input[..needed].chunks_exact(4) {
                let sample = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(sample as f32 / i32::MAX as f32);
            }
        }
    }

    Ok(())
}

fn convert_channels(input: &[f32], in_channels: u16, out_channels: u16, output: &mut Vec<f32>) {
    output.clear();

    if in_channels == out_channels {
        output.extend_from_slice(input);
        return;
    }

    let in_channels = usize::from(in_channels);
    let out_channels = usize::from(out_channels);

    if in_channels == 0 || out_channels == 0 {
        return;
    }

    let frame_count = input.len() / in_channels;
    output.reserve(frame_count * out_channels);

    for frame in input.chunks_exact(in_channels) {
        if out_channels == 1 {
            let sum: f32 = frame.iter().copied().sum();
            output.push(sum / in_channels as f32);
            continue;
        }

        if in_channels == 1 {
            for _ in 0..out_channels {
                output.push(frame[0]);
            }
            continue;
        }

        if out_channels <= in_channels {
            output.extend_from_slice(&frame[..out_channels]);
            continue;
        }

        for idx in 0..out_channels {
            let src = frame[idx % in_channels];
            output.push(src);
        }
    }
}

fn encode_from_f32(samples: &[f32], format: AudioSampleFormat) -> AudioResult<Vec<u8>> {
    match format {
        AudioSampleFormat::F32 => {
            let mut out = Vec::with_capacity(
                samples
                    .len()
                    .checked_mul(4)
                    .ok_or(AudioError::BufferOverflow)?,
            );
            for sample in samples {
                out.extend_from_slice(&sample.to_le_bytes());
            }
            Ok(out)
        }
        AudioSampleFormat::I16 => {
            let mut out = Vec::with_capacity(
                samples
                    .len()
                    .checked_mul(2)
                    .ok_or(AudioError::BufferOverflow)?,
            );
            for sample in samples {
                let clamped = sample.clamp(-1.0, 1.0);
                let scaled = (clamped * i16::MAX as f32) as i16;
                out.extend_from_slice(&scaled.to_le_bytes());
            }
            Ok(out)
        }
    }
}

struct LinearResampler {
    in_rate: u32,
    out_rate: u32,
    channels: u16,
    step: f64,
    position_in_frames: f64,
    input_buffer: Vec<f32>,
}

impl LinearResampler {
    fn new(in_rate: u32, out_rate: u32, channels: u16) -> Self {
        Self {
            in_rate,
            out_rate,
            channels,
            step: in_rate as f64 / out_rate as f64,
            position_in_frames: 0.0,
            input_buffer: Vec::new(),
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let channels = usize::from(self.channels);
        if channels == 0 {
            return;
        }

        self.input_buffer.extend_from_slice(input);
        let available_frames = self.input_buffer.len() / channels;

        if self.in_rate == self.out_rate {
            out.extend_from_slice(&self.input_buffer);
            self.input_buffer.clear();
            self.position_in_frames = 0.0;
            return;
        }

        while self.position_in_frames + 1.0 < available_frames as f64 {
            let i0 = self.position_in_frames.floor() as usize;
            let frac = (self.position_in_frames - i0 as f64) as f32;

            let base0 = i0 * channels;
            let base1 = (i0 + 1) * channels;
            for ch in 0..channels {
                let s0 = self.input_buffer[base0 + ch];
                let s1 = self.input_buffer[base1 + ch];
                out.push(s0 + (s1 - s0) * frac);
            }

            self.position_in_frames += self.step;
        }

        let consumed_frames = self.position_in_frames.floor() as usize;
        if consumed_frames > 0 {
            let consumed_samples = consumed_frames * channels;
            self.input_buffer.drain(0..consumed_samples);
            self.position_in_frames -= consumed_frames as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_i16_samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn converts_i16_and_clamps_when_encoding_i16() {
        let input = NativeAudioFormat {
            sample_rate: 48_000,
            channels: 1,
            sample_format: NativeSampleFormat::I16,
        };
        let output = AudioFormat::new(48_000, 1, AudioSampleFormat::I16);
        let mut converter = AudioConverter::new(input, output).unwrap();

        let mut source = Vec::new();
        source.extend_from_slice(&(-32768i16).to_le_bytes());
        source.extend_from_slice(&(0i16).to_le_bytes());
        source.extend_from_slice(&(32767i16).to_le_bytes());

        let encoded = converter.convert_chunk(&source, 3).unwrap();
        let samples = read_i16_samples(&encoded);
        assert_eq!(samples.len(), 3);
        assert!(samples[0] <= -32766);
        assert_eq!(samples[1], 0);
        assert!(samples[2] >= 32766);
    }

    #[test]
    fn converts_i32_and_f32_inputs() {
        let output = AudioFormat::new(48_000, 1, AudioSampleFormat::F32);

        let mut i32_converter = AudioConverter::new(
            NativeAudioFormat {
                sample_rate: 48_000,
                channels: 1,
                sample_format: NativeSampleFormat::I32,
            },
            output,
        )
        .unwrap();

        let mut i32_bytes = Vec::new();
        i32_bytes.extend_from_slice(&(i32::MAX).to_le_bytes());
        let i32_out = i32_converter.convert_chunk(&i32_bytes, 1).unwrap();
        let sample = f32::from_le_bytes([i32_out[0], i32_out[1], i32_out[2], i32_out[3]]);
        assert!(sample > 0.99);

        let mut f32_converter = AudioConverter::new(
            NativeAudioFormat {
                sample_rate: 48_000,
                channels: 1,
                sample_format: NativeSampleFormat::F32,
            },
            output,
        )
        .unwrap();
        let mut f32_bytes = Vec::new();
        f32_bytes.extend_from_slice(&0.5f32.to_le_bytes());
        let f32_out = f32_converter.convert_chunk(&f32_bytes, 1).unwrap();
        let sample = f32::from_le_bytes([f32_out[0], f32_out[1], f32_out[2], f32_out[3]]);
        assert!((sample - 0.5).abs() < 0.000_1);
    }

    #[test]
    fn channel_upmix_and_downmix_are_deterministic() {
        let input = vec![0.0f32, 1.0f32, 0.5f32, -0.5f32];
        let mut mono = Vec::new();
        convert_channels(&input, 2, 1, &mut mono);
        assert_eq!(mono, vec![0.5, 0.0]);

        let mut stereo = Vec::new();
        convert_channels(&mono, 1, 2, &mut stereo);
        assert_eq!(stereo, vec![0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn linear_resampler_preserves_continuity_across_calls() {
        let input = NativeAudioFormat {
            sample_rate: 48_000,
            channels: 1,
            sample_format: NativeSampleFormat::F32,
        };
        let output = AudioFormat::new(24_000, 1, AudioSampleFormat::F32);
        let mut converter = AudioConverter::new(input, output).unwrap();

        let mut first = Vec::new();
        for sample in [0.0f32, 1.0, 2.0, 3.0] {
            first.extend_from_slice(&sample.to_le_bytes());
        }
        let first_out = converter.convert_chunk(&first, 4).unwrap();
        let first_samples: Vec<f32> = first_out
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(first_samples.len(), 2);

        let mut second = Vec::new();
        for sample in [4.0f32, 5.0, 6.0, 7.0] {
            second.extend_from_slice(&sample.to_le_bytes());
        }
        let second_out = converter.convert_chunk(&second, 4).unwrap();
        let second_samples: Vec<f32> = second_out
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert!(!second_samples.is_empty());
        assert!(second_samples[0] >= first_samples.last().copied().unwrap_or(0.0));
    }
}
