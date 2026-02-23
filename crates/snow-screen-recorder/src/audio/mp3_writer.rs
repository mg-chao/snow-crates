use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use shine_rs::{Mp3Encoder, Mp3EncoderConfig, StereoMode};

use crate::audio::sanitize_samples_for_mp3;
use crate::error::{Result, ScreenRecorderError};

pub struct Mp3FileWriter {
    encoder: Mp3Encoder,
    writer: BufWriter<File>,
    channels: usize,
}

impl Mp3FileWriter {
    pub fn create(
        path: &Path,
        sample_rate_hz: u32,
        channels: u16,
        bitrate_kbps: u16,
    ) -> Result<Self> {
        let stereo_mode = if channels == 1 {
            StereoMode::Mono
        } else {
            StereoMode::Stereo
        };

        let config = Mp3EncoderConfig::new()
            .sample_rate(sample_rate_hz)
            .channels(channels as u8)
            .bitrate(bitrate_kbps as u32)
            .stereo_mode(stereo_mode);

        let encoder = Mp3Encoder::new(config)
            .map_err(|e| ScreenRecorderError::Encode(format!("failed to init mp3 encoder: {e}")))?;
        let writer = BufWriter::new(File::create(path)?);

        Ok(Self {
            encoder,
            writer,
            channels: usize::from(channels),
        })
    }

    pub fn append_i16_le_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        if bytes.len() % 2 != 0 {
            return Err(ScreenRecorderError::Encode(
                "PCM bytes must be i16 aligned".to_string(),
            ));
        }

        let mut samples = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks_exact(2) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        sanitize_samples_for_mp3(&mut samples);

        if samples.len() % self.channels != 0 {
            return Err(ScreenRecorderError::Encode(format!(
                "PCM samples are not channel-aligned ({} samples for {} channels)",
                samples.len(),
                self.channels
            )));
        }

        let encoded_frames = self
            .encoder
            .encode_interleaved(&samples)
            .map_err(|e| ScreenRecorderError::Encode(format!("mp3 encode failed: {e}")))?;

        for frame in encoded_frames {
            self.writer.write_all(&frame)?;
        }

        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        let tail = self
            .encoder
            .finish()
            .map_err(|e| ScreenRecorderError::Encode(format!("mp3 finalize failed: {e}")))?;

        if !tail.is_empty() {
            self.writer.write_all(&tail)?;
        }

        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn writer_accepts_min_i16_samples() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("snow-mp3-writer-{suffix}.mp3"));

        let mut writer = Mp3FileWriter::create(&path, 48_000, 2, 192).unwrap();
        let mut pcm = Vec::new();
        for _ in 0..4_096 {
            pcm.extend_from_slice(&i16::MIN.to_le_bytes());
            pcm.extend_from_slice(&i16::MIN.to_le_bytes());
        }
        writer.append_i16_le_bytes(&pcm).unwrap();
        writer.finish().unwrap();

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 0);
        let _ = std::fs::remove_file(path);
    }
}
