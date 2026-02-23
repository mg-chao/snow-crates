use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ffmpeg_next as ffmpeg;

use crate::config::RecordingAudioFormat;
use crate::error::{Result, ScreenRecorderError};

const INPUT_SAMPLE_FORMAT: ffmpeg::format::Sample =
    ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed);
const FALLBACK_FRAME_SAMPLES: usize = 1152;

fn ensure_ffmpeg_initialized() -> std::result::Result<(), String> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| ffmpeg::init().map_err(|err| err.to_string()))
        .clone()
}

fn ensure_ffmpeg_encode() -> Result<()> {
    ensure_ffmpeg_initialized()
        .map_err(|err| ScreenRecorderError::Encode(format!("failed to initialize ffmpeg: {err}")))
}

fn is_eagain(err: &ffmpeg::Error) -> bool {
    matches!(
        err,
        ffmpeg::Error::Other { errno } if *errno == ffmpeg::error::EAGAIN
    )
}

fn choose_mp3_encoder() -> Option<ffmpeg::Codec> {
    ffmpeg::encoder::find_by_name("libmp3lame")
        .or_else(|| ffmpeg::encoder::find_by_name("libshine"))
        .or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::MP3))
}

fn choose_aac_encoder() -> Option<ffmpeg::Codec> {
    ffmpeg::encoder::find_by_name("aac").or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::AAC))
}

fn choose_sample_rate(codec: ffmpeg::codec::Audio, requested_hz: u32) -> u32 {
    if let Some(mut rates) = codec.rates() {
        let available: Vec<u32> = rates.by_ref().map(|rate| rate.max(1) as u32).collect();
        if available.is_empty() {
            return requested_hz.max(1);
        }
        if available.contains(&requested_hz) {
            return requested_hz;
        }
        return available
            .into_iter()
            .min_by_key(|rate| rate.abs_diff(requested_hz))
            .unwrap_or(requested_hz.max(1));
    }
    requested_hz.max(1)
}

fn choose_sample_format(codec: ffmpeg::codec::Audio) -> ffmpeg::format::Sample {
    let preferred = [
        ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
        ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Planar),
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar),
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
    ];

    if let Some(formats) = codec.formats() {
        let available: Vec<_> = formats.collect();
        for candidate in preferred {
            if available.iter().any(|fmt| *fmt == candidate) {
                return candidate;
            }
        }
        if let Some(first) = available.first().copied() {
            return first;
        }
    }

    INPUT_SAMPLE_FORMAT
}

fn choose_channel_layout(
    codec: ffmpeg::codec::Audio,
    requested_channels: u16,
) -> ffmpeg::ChannelLayout {
    codec
        .channel_layouts()
        .map(|layouts| layouts.best(i32::from(requested_channels.max(1))))
        .unwrap_or_else(|| ffmpeg::ChannelLayout::default(i32::from(requested_channels.max(1))))
}

#[derive(Clone, Debug)]
pub struct AudioWriterConfig {
    pub output_path: PathBuf,
    pub format: RecordingAudioFormat,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bitrate_kbps: u16,
}

impl AudioWriterConfig {
    pub fn validate(&self) -> Result<()> {
        if self.output_path.as_os_str().is_empty() {
            return Err(ScreenRecorderError::InvalidConfig(
                "audio output path must not be empty".to_string(),
            ));
        }
        if self.channels == 0 {
            return Err(ScreenRecorderError::InvalidConfig(
                "audio writer requires at least 1 channel".to_string(),
            ));
        }
        if self.sample_rate_hz == 0 {
            return Err(ScreenRecorderError::InvalidConfig(
                "audio sample rate must be > 0".to_string(),
            ));
        }
        if self.bitrate_kbps == 0 {
            return Err(ScreenRecorderError::InvalidConfig(
                "audio bitrate must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

pub struct AudioFileWriter {
    output: ffmpeg::format::context::Output,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    encoder: ffmpeg::encoder::audio::Encoder,
    resampler: ffmpeg::software::resampling::Context,
    input_channels: usize,
    input_rate_hz: u32,
    input_layout: ffmpeg::ChannelLayout,
    frame_samples: usize,
    variable_frame_size: bool,
    pcm_buffer: Vec<u8>,
    next_pts: i64,
}

impl AudioFileWriter {
    pub fn create(config: AudioWriterConfig) -> Result<Self> {
        ensure_ffmpeg_encode()?;
        config.validate()?;

        let path: &Path = &config.output_path;
        let format = config.format;
        let sample_rate_hz = config.sample_rate_hz;
        let channels = config.channels;
        let bitrate_kbps = config.bitrate_kbps;

        let codec = match format {
            RecordingAudioFormat::Mp3 => choose_mp3_encoder(),
            RecordingAudioFormat::Aac => choose_aac_encoder(),
        }
        .ok_or_else(|| {
            let label = match format {
                RecordingAudioFormat::Mp3 => "MP3",
                RecordingAudioFormat::Aac => "AAC",
            };
            ScreenRecorderError::Encode(format!("no {label} encoder available in ffmpeg"))
        })?;
        let codec_audio = codec.audio().map_err(|err| {
            ScreenRecorderError::Encode(format!(
                "failed to use {} codec as audio encoder: {err}",
                codec.name()
            ))
        })?;

        let input_rate_hz = sample_rate_hz.max(1);
        let output_rate_hz = choose_sample_rate(codec_audio, input_rate_hz);
        let input_layout = ffmpeg::ChannelLayout::default(i32::from(channels.max(1)));
        let output_layout = choose_channel_layout(codec_audio, channels);
        let output_format = choose_sample_format(codec_audio);

        let mut output = ffmpeg::format::output(path).map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to create audio output context: {err}"))
        })?;

        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);

        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(|err| {
                ScreenRecorderError::Encode(format!(
                    "failed to create audio encoder context: {err}"
                ))
            })?;

        encoder.set_rate(output_rate_hz as i32);
        encoder.set_channel_layout(output_layout);
        encoder.set_format(output_format);
        encoder.set_bit_rate(usize::from(bitrate_kbps.max(8)) * 1000);
        encoder.set_time_base((1, output_rate_hz as i32));

        if global_header {
            encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }

        let codec_name = codec.name().to_ascii_lowercase();
        let encoder = if codec_name == "libmp3lame" {
            let mut options = ffmpeg::Dictionary::new();
            options.set("compression_level", "0");
            options.set("reservoir", "1");

            encoder.open_as_with(codec, options).map_err(|err| {
                ScreenRecorderError::Encode(format!("failed to open audio encoder: {err}"))
            })?
        } else if codec_name == "aac" {
            let mut options = ffmpeg::Dictionary::new();
            options.set("profile", "aac_low");

            encoder.open_as_with(codec, options).map_err(|err| {
                ScreenRecorderError::Encode(format!("failed to open audio encoder: {err}"))
            })?
        } else {
            encoder.open_as(codec).map_err(|err| {
                ScreenRecorderError::Encode(format!("failed to open audio encoder: {err}"))
            })?
        };

        let variable_frame_size = codec
            .capabilities()
            .contains(ffmpeg::codec::capabilities::Capabilities::VARIABLE_FRAME_SIZE);
        let mut frame_samples = encoder.frame_size() as usize;
        if frame_samples == 0 {
            frame_samples = FALLBACK_FRAME_SAMPLES;
        }

        let stream_time_base = ffmpeg::Rational(1, output_rate_hz as i32);
        let stream_index = {
            let mut stream = output.add_stream(codec).map_err(|err| {
                ScreenRecorderError::Encode(format!("failed to add audio stream: {err}"))
            })?;
            stream.set_time_base(stream_time_base);
            stream.set_rate(ffmpeg::Rational(output_rate_hz as i32, 1));
            stream.set_parameters(&encoder);
            stream.index()
        };

        output.write_header().map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to write audio header: {err}"))
        })?;
        let stream_time_base = output
            .stream(stream_index)
            .map(|stream| stream.time_base())
            .ok_or_else(|| {
                ScreenRecorderError::Encode(format!(
                    "failed to resolve output audio stream {} after header",
                    stream_index
                ))
            })?;

        let resampler = ffmpeg::software::resampling::Context::get(
            INPUT_SAMPLE_FORMAT,
            input_layout,
            input_rate_hz,
            encoder.format(),
            encoder.channel_layout(),
            encoder.rate(),
        )
        .map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to create audio resampler: {err}"))
        })?;

        Ok(Self {
            output,
            stream_index,
            stream_time_base,
            encoder,
            resampler,
            input_channels: usize::from(channels),
            input_rate_hz,
            input_layout,
            frame_samples,
            variable_frame_size,
            pcm_buffer: Vec::with_capacity(frame_samples * usize::from(channels) * 2),
            next_pts: 0,
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

        let sample_count = bytes.len() / 2;
        if sample_count % self.input_channels != 0 {
            return Err(ScreenRecorderError::Encode(format!(
                "PCM samples are not channel-aligned ({} samples for {} channels)",
                sample_count, self.input_channels
            )));
        }

        self.pcm_buffer.extend_from_slice(bytes);
        self.encode_ready_chunks(false)
    }

    fn encode_ready_chunks(&mut self, finalizing: bool) -> Result<()> {
        let frame_bytes = self
            .frame_samples
            .saturating_mul(self.input_channels)
            .saturating_mul(2);
        let variable_chunk_bytes = ((self.input_rate_hz as usize / 25).max(1))
            .saturating_mul(self.input_channels)
            .saturating_mul(2);

        if self.variable_frame_size {
            while self.pcm_buffer.len() >= variable_chunk_bytes {
                let chunk: Vec<u8> = self.pcm_buffer.drain(..variable_chunk_bytes).collect();
                self.encode_pcm_chunk(&chunk)?;
            }
            if finalizing && !self.pcm_buffer.is_empty() {
                let chunk = std::mem::take(&mut self.pcm_buffer);
                self.encode_pcm_chunk(&chunk)?;
            }
            return Ok(());
        }

        while self.pcm_buffer.len() >= frame_bytes {
            let chunk: Vec<u8> = self.pcm_buffer.drain(..frame_bytes).collect();
            self.encode_pcm_chunk(&chunk)?;
        }

        if finalizing && !self.pcm_buffer.is_empty() {
            let mut chunk = std::mem::take(&mut self.pcm_buffer);
            let missing = frame_bytes.saturating_sub(chunk.len());
            if missing > 0 {
                chunk.resize(chunk.len() + missing, 0);
            }
            self.encode_pcm_chunk(&chunk)?;
        }

        Ok(())
    }

    fn encode_pcm_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }

        let sample_count = chunk.len() / 2;
        if sample_count % self.input_channels != 0 {
            return Err(ScreenRecorderError::Encode(
                "PCM chunk is not channel-aligned".to_string(),
            ));
        }
        let samples_per_channel = sample_count / self.input_channels;
        let mut source =
            ffmpeg::frame::Audio::new(INPUT_SAMPLE_FORMAT, samples_per_channel, self.input_layout);
        source.set_rate(self.input_rate_hz);
        source.data_mut(0)[..chunk.len()].copy_from_slice(chunk);

        let mut converted = ffmpeg::frame::Audio::empty();
        self.resampler.run(&source, &mut converted).map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to resample audio chunk: {err}"))
        })?;

        if converted.samples() == 0 {
            return Ok(());
        }
        self.send_audio_frame(&mut converted)
    }

    fn send_audio_frame(&mut self, frame: &mut ffmpeg::frame::Audio) -> Result<()> {
        frame.set_pts(Some(self.next_pts));
        self.next_pts = self.next_pts.saturating_add(frame.samples() as i64);

        self.encoder.send_frame(frame).map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to send frame to audio encoder: {err}"))
        })?;
        self.drain_packets(false)
    }

    fn drain_resampler(&mut self) -> Result<()> {
        loop {
            let mut frame = ffmpeg::frame::Audio::new(
                self.encoder.format(),
                self.frame_samples.max(1),
                self.encoder.channel_layout(),
            );
            frame.set_rate(self.encoder.rate());
            let delay = match self.resampler.flush(&mut frame) {
                Ok(delay) => delay,
                Err(ffmpeg::Error::OutputChanged) => break,
                Err(err) => {
                    return Err(ScreenRecorderError::Encode(format!(
                        "failed to flush audio resampler: {err}"
                    )));
                }
            };

            if frame.samples() > 0 {
                self.send_audio_frame(&mut frame)?;
            }

            if !delay.is_some_and(|d| d.output > 0) {
                break;
            }
        }
        Ok(())
    }

    fn drain_packets(&mut self, draining: bool) -> Result<()> {
        loop {
            let mut packet = ffmpeg::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.stream_index);
                    packet.rescale_ts(self.encoder.time_base(), self.stream_time_base);
                    packet.write_interleaved(&mut self.output).map_err(|err| {
                        ScreenRecorderError::Encode(format!("failed to write audio packet: {err}"))
                    })?;
                }
                Err(err) if err == ffmpeg::Error::Eof => break,
                Err(err) if is_eagain(&err) && !draining => break,
                Err(err) if is_eagain(&err) && draining => continue,
                Err(err) => {
                    return Err(ScreenRecorderError::Encode(format!(
                        "failed to receive audio packet: {err}"
                    )));
                }
            }
        }

        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.encode_ready_chunks(true)?;
        self.drain_resampler()?;

        self.encoder.send_eof().map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to finalize audio encoder: {err}"))
        })?;
        self.drain_packets(true)?;

        self.output.write_trailer().map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to write audio trailer: {err}"))
        })?;
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

        let mut writer = AudioFileWriter::create(AudioWriterConfig {
            output_path: path.clone(),
            format: RecordingAudioFormat::Mp3,
            sample_rate_hz: 48_000,
            channels: 2,
            bitrate_kbps: 192,
        })
        .unwrap();
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

    #[test]
    fn writer_accepts_aac_format() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("snow-aac-writer-{suffix}.aac"));

        let mut writer = AudioFileWriter::create(AudioWriterConfig {
            output_path: path.clone(),
            format: RecordingAudioFormat::Aac,
            sample_rate_hz: 48_000,
            channels: 2,
            bitrate_kbps: 160,
        })
        .unwrap();
        let mut pcm = Vec::new();
        for _ in 0..4_096 {
            pcm.extend_from_slice(&0i16.to_le_bytes());
            pcm.extend_from_slice(&0i16.to_le_bytes());
        }
        writer.append_i16_le_bytes(&pcm).unwrap();
        writer.finish().unwrap();

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 0);
        let _ = std::fs::remove_file(path);
    }
}
