use crate::error::{AudioError, AudioResult};

/// Maximum number of audio channels supported by this crate.
///
/// This constant is used both for validation in [`AudioFormat::validate`] and
/// as the upper bound for stack-allocated channel buffers in the conversion
/// pipeline. Changing it here automatically updates both limits.
pub const MAX_CHANNELS: u16 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSampleFormat {
    F32,
    I16,
}

impl AudioSampleFormat {
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::I16 => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: AudioSampleFormat,
}

impl AudioFormat {
    pub const fn new(sample_rate: u32, channels: u16, sample_format: AudioSampleFormat) -> Self {
        Self {
            sample_rate,
            channels,
            sample_format,
        }
    }

    pub fn validate(&self) -> AudioResult<()> {
        if self.sample_rate == 0 {
            return Err(AudioError::InvalidConfig(
                "sample rate must be greater than zero".into(),
            ));
        }
        if self.channels == 0 {
            return Err(AudioError::InvalidConfig(
                "channel count must be greater than zero".into(),
            ));
        }
        if self.channels > MAX_CHANNELS {
            return Err(AudioError::InvalidConfig(
                format!("channel count above {MAX_CHANNELS} is not supported"),
            ));
        }
        Ok(())
    }

    pub fn bytes_per_frame(&self) -> AudioResult<usize> {
        self.validate()?;
        let channels = usize::from(self.channels);
        channels
            .checked_mul(self.sample_format.bytes_per_sample())
            .ok_or(AudioError::BufferOverflow)
    }

    pub fn bytes_for_frames(&self, frames: u32) -> AudioResult<usize> {
        let frame_bytes = self.bytes_per_frame()?;
        frame_bytes
            .checked_mul(frames as usize)
            .ok_or(AudioError::BufferOverflow)
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            sample_format: AudioSampleFormat::F32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_invalid_inputs() {
        assert!(AudioFormat::new(0, 2, AudioSampleFormat::F32).validate().is_err());
        assert!(AudioFormat::new(48_000, 0, AudioSampleFormat::F32)
            .validate()
            .is_err());
        assert!(AudioFormat::new(48_000, 64, AudioSampleFormat::F32)
            .validate()
            .is_err());
    }

    #[test]
    fn byte_size_computation_matches_expectation() {
        let fmt = AudioFormat::new(48_000, 2, AudioSampleFormat::I16);
        assert_eq!(fmt.bytes_per_frame().unwrap(), 4);
        assert_eq!(fmt.bytes_for_frames(480).unwrap(), 1_920);
    }
}
