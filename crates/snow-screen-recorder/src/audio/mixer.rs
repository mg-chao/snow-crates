use std::fs::File;
use std::path::PathBuf;

use minimp3_fixed::{Decoder, Error as Mp3DecodeError, Frame};
use shine_rs::{Mp3Encoder, Mp3EncoderConfig, StereoMode};

use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Debug)]
pub struct Mp3SourceInput {
    pub path: PathBuf,
    pub enabled: bool,
    pub volume: f32,
}

#[derive(Clone, Debug)]
struct AudioBufferF32 {
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

fn decode_mp3(path: &std::path::Path) -> Result<AudioBufferF32> {
    let file = File::open(path)?;
    let mut decoder = Decoder::new(file);
    let mut out = Vec::<f32>::new();
    let mut sample_rate = 48_000u32;
    let mut channels = 2u16;

    loop {
        match decoder.next_frame() {
            Ok(Frame {
                data,
                sample_rate: sr,
                channels: ch,
                ..
            }) => {
                sample_rate = sr as u32;
                channels = ch as u16;
                out.extend(data.into_iter().map(|v| v as f32 / 32768.0));
            }
            Err(Mp3DecodeError::Eof) => break,
            Err(e) => {
                return Err(ScreenRecorderError::Decode(format!(
                    "failed to decode mp3 {}: {e}",
                    path.display()
                )));
            }
        }
    }

    Ok(AudioBufferF32 {
        sample_rate,
        channels,
        samples: out,
    })
}

fn remap_channels(samples: &[f32], in_channels: u16, out_channels: u16) -> Vec<f32> {
    if in_channels == out_channels {
        return samples.to_vec();
    }

    let in_ch = in_channels as usize;
    let out_ch = out_channels as usize;
    let frame_count = samples.len() / in_ch;
    let mut out = Vec::with_capacity(frame_count * out_ch);

    for frame in samples.chunks_exact(in_ch) {
        if out_ch == 1 {
            let sum: f32 = frame.iter().copied().sum();
            out.push(sum / in_ch as f32);
            continue;
        }

        if in_ch == 1 {
            out.extend(std::iter::repeat_n(frame[0], out_ch));
            continue;
        }

        for c in 0..out_ch {
            out.push(frame[c % in_ch]);
        }
    }

    out
}

fn resample_with_speed(
    samples: &[f32],
    src_rate: u32,
    dst_rate: u32,
    channels: u16,
    speed: f32,
) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    let channels = channels as usize;
    let in_frames = samples.len() / channels;
    let speed = speed.max(0.01);
    let ratio = (src_rate as f64 / dst_rate as f64) * speed as f64;
    let out_frames = ((in_frames as f64) / ratio).floor().max(1.0) as usize;

    let mut out = vec![0.0f32; out_frames * channels];

    for dst_frame in 0..out_frames {
        let src_pos = dst_frame as f64 * ratio;
        let src_index = src_pos.floor() as usize;
        let frac = (src_pos - src_index as f64) as f32;
        let src_index_next = (src_index + 1).min(in_frames.saturating_sub(1));

        for ch in 0..channels {
            let a = samples[src_index * channels + ch];
            let b = samples[src_index_next * channels + ch];
            out[dst_frame * channels + ch] = a + (b - a) * frac;
        }
    }

    out
}

pub fn mix_mp3_sources_to_pcm(
    system: Option<Mp3SourceInput>,
    microphone: Option<Mp3SourceInput>,
    playback_speed: f32,
    target_sample_rate_hz: u32,
    target_channels: u16,
) -> Result<Vec<i16>> {
    let mut prepared_tracks = Vec::new();

    for input in [system, microphone].into_iter().flatten() {
        if !input.enabled {
            continue;
        }

        let mut decoded = decode_mp3(&input.path)?;
        decoded.samples = remap_channels(&decoded.samples, decoded.channels, target_channels);
        decoded.channels = target_channels;
        decoded.samples = resample_with_speed(
            &decoded.samples,
            decoded.sample_rate,
            target_sample_rate_hz,
            target_channels,
            playback_speed,
        );
        decoded.sample_rate = target_sample_rate_hz;

        if (input.volume - 1.0).abs() > f32::EPSILON {
            for s in &mut decoded.samples {
                *s *= input.volume;
            }
        }

        prepared_tracks.push(decoded);
    }

    if prepared_tracks.is_empty() {
        return Ok(Vec::new());
    }

    let channels = target_channels as usize;
    let max_samples = prepared_tracks
        .iter()
        .map(|t| t.samples.len())
        .max()
        .unwrap_or(0);

    let mut mixed = vec![0.0f32; max_samples];
    for track in &prepared_tracks {
        for (idx, value) in track.samples.iter().enumerate() {
            mixed[idx] += *value;
        }
    }

    // Soft-normalize by track count to reduce clipping.
    let norm = prepared_tracks.len() as f32;
    for value in &mut mixed {
        *value = (*value / norm).clamp(-1.0, 1.0);
    }

    let mut pcm = Vec::with_capacity(mixed.len());
    pcm.extend(mixed.into_iter().map(|v| (v * 32767.0) as i16));

    // Ensure i16 buffer is frame aligned.
    let remainder = pcm.len() % channels;
    if remainder != 0 {
        pcm.truncate(pcm.len() - remainder);
    }

    Ok(pcm)
}

pub fn encode_pcm_to_mp3(
    pcm_interleaved: &[i16],
    sample_rate_hz: u32,
    channels: u16,
    bitrate_kbps: u16,
) -> Result<Vec<u8>> {
    if pcm_interleaved.is_empty() {
        return Ok(Vec::new());
    }

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

    let mut encoder = Mp3Encoder::new(config)
        .map_err(|e| ScreenRecorderError::Encode(format!("failed to init mp3 encoder: {e}")))?;

    let frame_samples = encoder.samples_per_frame();
    let mut output = Vec::new();
    for chunk in pcm_interleaved.chunks(frame_samples) {
        let encoded = encoder
            .encode_interleaved(chunk)
            .map_err(|e| ScreenRecorderError::Encode(format!("mp3 encode failed: {e}")))?;
        for frame in encoded {
            output.extend_from_slice(&frame);
        }
    }

    let tail = encoder
        .finish()
        .map_err(|e| ScreenRecorderError::Encode(format!("mp3 finalize failed: {e}")))?;
    output.extend_from_slice(&tail);
    Ok(output)
}
