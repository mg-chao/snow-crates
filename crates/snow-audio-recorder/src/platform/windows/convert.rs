use crate::error::{AudioError, AudioResult};
use crate::format::{AudioFormat, AudioSampleFormat, MAX_CHANNELS};

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

/// Trait for sample-rate conversion implementations.
///
/// Implementations must be stateful to handle continuity across chunk
/// boundaries (the resampler may buffer trailing samples from one call and
/// prepend them to the next).
pub(crate) trait Resampler {
    /// Resample interleaved `f32` samples from `input` and append the result
    /// to `out`. The number of channels is fixed at construction time.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>);
}

/// Selects which resampler implementation to use.
///
/// Using an enum rather than `Box<dyn Resampler>` keeps the hot path
/// monomorphised and avoids a heap allocation for the trait object.
pub(crate) enum ResamplerKind {
    Linear(LinearResampler),
}

impl Resampler for ResamplerKind {
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        match self {
            Self::Linear(inner) => inner.process(input, out),
        }
    }
}

pub(crate) struct AudioConverter {
    input: NativeAudioFormat,
    output: AudioFormat,
    decode_buffer: Vec<f32>,
    channel_buffer: Vec<f32>,
    resample_buffer: Vec<f32>,
    encode_buffer: Vec<u8>,
    resampler: Option<ResamplerKind>,
}

impl AudioConverter {
    /// Create a converter that uses the default [`LinearResampler`] when the
    /// input and output sample rates differ.
    pub fn new(input: NativeAudioFormat, output: AudioFormat) -> AudioResult<Self> {
        let resampler = if input.sample_rate == output.sample_rate {
            None
        } else {
            Some(ResamplerKind::Linear(LinearResampler::new(
                input.sample_rate,
                output.sample_rate,
                output.channels,
            )))
        };
        Self::with_resampler(input, output, resampler)
    }

    /// Create a converter with an explicit resampler. Pass `None` to disable
    /// resampling (the caller is responsible for ensuring rates match).
    pub fn with_resampler(
        input: NativeAudioFormat,
        output: AudioFormat,
        resampler: Option<ResamplerKind>,
    ) -> AudioResult<Self> {
        output.validate()?;
        Ok(Self {
            input,
            output,
            decode_buffer: Vec::new(),
            channel_buffer: Vec::new(),
            resample_buffer: Vec::new(),
            encode_buffer: Vec::new(),
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

        encode_from_f32_into(
            output_samples,
            self.output.sample_format,
            &mut self.encode_buffer,
        )?;
        Ok(std::mem::take(&mut self.encode_buffer))
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

/// Map channels between different channel counts.
///
/// The following conversions are well-defined:
///
/// - **Same count** (`in == out`): pass-through.
/// - **Downmix to mono** (`out == 1`): average all input channels.
/// - **Upmix from mono** (`in == 1`): duplicate to every output channel.
///
/// # Limitations - arbitrary surround layouts
///
/// For all other combinations the function applies a *naive* strategy that
/// does **not** consult speaker-position masks:
///
/// - **Downmix** (`out < in`): each output channel is the average of the
///   input channels that map to it via round-robin (`idx % out_channels`).
///   This preserves energy better than simple truncation but does not
///   implement ITU / Dolby fold-down coefficients, so spatial information
///   from surround layouts (e.g. 5.1 -> stereo) will be mixed incorrectly.
///
/// - **Upmix** (`out > in`): extra output channels are filled by cycling
///   through the input channels (`idx % in_channels`). This is essentially
///   channel duplication and will not produce correct surround placement.
///
/// If accurate surround-to-stereo (or vice-versa) conversion is required,
/// a dedicated channel mapping stage with proper fold-down coefficients
/// should be used instead of this function.
fn convert_channels(input: &[f32], in_channels: u16, out_channels: u16, output: &mut Vec<f32>) {
    output.clear();

    if in_channels == out_channels {
        output.extend_from_slice(input);
        return;
    }

    let in_ch = usize::from(in_channels);
    let out_ch = usize::from(out_channels);

    if in_ch == 0 || out_ch == 0 {
        return;
    }

    let frame_count = input.len() / in_ch;
    output.reserve(frame_count * out_ch);

    for frame in input.chunks_exact(in_ch) {
        // Downmix to mono - average all input channels.
        if out_ch == 1 {
            let sum: f32 = frame.iter().copied().sum();
            output.push(sum / in_ch as f32);
            continue;
        }

        // Upmix from mono - duplicate to every output channel.
        if in_ch == 1 {
            for _ in 0..out_ch {
                output.push(frame[0]);
            }
            continue;
        }

        // Arbitrary downmix (out < in): round-robin average.
        // Each output channel accumulates the input channels that map to it
        // and divides by the count, preserving overall energy.
        if out_ch < in_ch {
            // Accumulate into a small stack buffer sized to the crate-wide
            // channel limit so it stays in sync with validation.
            let mut accum = [0.0f32; MAX_CHANNELS as usize];
            let mut count = [0u32; MAX_CHANNELS as usize];
            for (idx, &sample) in frame.iter().enumerate() {
                let dest = idx % out_ch;
                accum[dest] += sample;
                count[dest] += 1;
            }
            for ch in 0..out_ch {
                output.push(if count[ch] > 0 {
                    accum[ch] / count[ch] as f32
                } else {
                    0.0
                });
            }
            continue;
        }

        // Arbitrary upmix (out > in): cycle through input channels.
        for idx in 0..out_ch {
            let src = frame[idx % in_ch];
            output.push(src);
        }
    }
}

fn encode_from_f32_into(
    samples: &[f32],
    format: AudioSampleFormat,
    out: &mut Vec<u8>,
) -> AudioResult<()> {
    out.clear();
    match format {
        AudioSampleFormat::F32 => {
            out.reserve(
                samples
                    .len()
                    .checked_mul(4)
                    .ok_or(AudioError::BufferOverflow)?,
            );
            for sample in samples {
                out.extend_from_slice(&sample.to_le_bytes());
            }
        }
        AudioSampleFormat::I16 => {
            out.reserve(
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
        }
    }
    Ok(())
}

/// A simple linear-interpolation resampler.
///
/// This is the cheapest possible sample-rate converter: it walks through the
/// input at a fractional step and linearly interpolates between adjacent
/// samples. It introduces audible aliasing at large rate ratios and should
/// be considered a baseline implementation.
///
/// For higher quality, implement the [`Resampler`] trait with a windowed-sinc
/// or polyphase filter and pass it via [`AudioConverter::with_resampler`].
pub(crate) struct LinearResampler {
    in_rate: u32,
    out_rate: u32,
    channels: u16,
    step: f64,
    position_in_frames: f64,
    input_buffer: Vec<f32>,
}

impl LinearResampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: u16) -> Self {
        Self {
            in_rate,
            out_rate,
            channels,
            step: in_rate as f64 / out_rate as f64,
            position_in_frames: 0.0,
            input_buffer: Vec::new(),
        }
    }
}

impl Resampler for LinearResampler {
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
    fn arbitrary_downmix_averages_round_robin() {
        let input = vec![1.0f32, 2.0, 3.0, 4.0];
        let mut out = Vec::new();
        convert_channels(&input, 4, 2, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 2.0).abs() < 1e-6);
        assert!((out[1] - 3.0).abs() < 1e-6);
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

    #[test]
    fn with_resampler_accepts_explicit_resampler() {
        let input = NativeAudioFormat {
            sample_rate: 44_100,
            channels: 1,
            sample_format: NativeSampleFormat::F32,
        };
        let output = AudioFormat::new(48_000, 1, AudioSampleFormat::F32);
        let resampler = ResamplerKind::Linear(LinearResampler::new(44_100, 48_000, 1));
        let mut converter = AudioConverter::with_resampler(input, output, Some(resampler)).unwrap();

        let mut src = Vec::new();
        for s in [0.0f32, 0.5, 1.0, 0.5] {
            src.extend_from_slice(&s.to_le_bytes());
        }
        let out = converter.convert_chunk(&src, 4).unwrap();
        assert!(!out.is_empty());
    }
}
