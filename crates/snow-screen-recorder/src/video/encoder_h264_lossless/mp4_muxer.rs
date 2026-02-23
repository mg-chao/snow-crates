use std::path::PathBuf;

use super::*;

const DEFAULT_AUDIO_FRAME_SAMPLES: usize = 1024;
const DEFAULT_AUDIO_BITRATE_KBPS: u16 = 192;

#[derive(Clone, Debug)]
pub struct AudioMixTrack {
    pub path: PathBuf,
    pub volume: f32,
}

#[derive(Clone, Debug)]
pub struct Mp4AudioMixConfig {
    pub tracks: Vec<AudioMixTrack>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bitrate_kbps: u16,
    pub playback_speed: f32,
    pub trim_start_ms: u64,
    pub trim_duration_ms: Option<u64>,
}

#[derive(Clone, Debug)]
struct MixedPcmAudio {
    sample_rate_hz: u32,
    channels: u16,
    samples_i16: Vec<i16>,
}

fn choose_audio_sample_rate(codec: ffmpeg::codec::Audio, requested_hz: u32) -> u32 {
    if let Some(rates) = codec.rates() {
        let available: Vec<u32> = rates.map(|rate| rate.max(1) as u32).collect();
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

fn choose_audio_channel_layout(
    codec: ffmpeg::codec::Audio,
    requested_channels: u16,
) -> ffmpeg::ChannelLayout {
    codec
        .channel_layouts()
        .map(|layouts| layouts.best(i32::from(requested_channels.max(1))))
        .unwrap_or_else(|| ffmpeg::ChannelLayout::default(i32::from(requested_channels.max(1))))
}

fn choose_audio_sample_format(codec: ffmpeg::codec::Audio) -> ffmpeg::format::Sample {
    let preferred = [
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar),
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
        ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Planar),
        ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
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

    ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed)
}

fn append_audio_frame_i16_samples(
    frame: &ffmpeg::frame::Audio,
    channels: u16,
    out: &mut Vec<i16>,
) -> Result<()> {
    let samples = frame.samples();
    if samples == 0 {
        return Ok(());
    }

    let channels_usize = usize::from(channels.max(1));
    let expected_bytes = samples
        .checked_mul(channels_usize)
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| ScreenRecorderError::Decode("audio frame size overflow".to_string()))?;
    let plane = frame.data(0);
    if plane.len() < expected_bytes {
        return Err(ScreenRecorderError::Decode(format!(
            "decoded audio frame is shorter than expected (have {}, need {})",
            plane.len(),
            expected_bytes
        )));
    }

    out.extend(
        plane[..expected_bytes]
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]])),
    );
    Ok(())
}

fn decode_audio_file_to_i16_samples(
    path: &Path,
    target_rate_hz: u32,
    target_channels: u16,
) -> Result<Vec<i16>> {
    ensure_ffmpeg_decode()?;

    let mut input = ffmpeg::format::input(path).map_err(|err| {
        ScreenRecorderError::Decode(format!(
            "failed to open audio file {}: {err}",
            path.display()
        ))
    })?;
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .ok_or_else(|| ScreenRecorderError::Decode("no audio stream found".to_string()))?;
    let stream_index = stream.index();

    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|err| {
            ScreenRecorderError::Decode(format!("failed to init audio decoder context: {err}"))
        })?
        .decoder()
        .audio()
        .map_err(|err| {
            ScreenRecorderError::Decode(format!("failed to open audio decoder: {err}"))
        })?;

    let input_layout = {
        let layout = decoder.channel_layout();
        if layout.is_empty() {
            ffmpeg::ChannelLayout::default(i32::from(decoder.channels().max(1)))
        } else {
            layout
        }
    };
    let output_layout = ffmpeg::ChannelLayout::default(i32::from(target_channels.max(1)));
    let mut resampler = ffmpeg::software::resampling::Context::get(
        decoder.format(),
        input_layout,
        decoder.rate(),
        ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
        output_layout,
        target_rate_hz.max(1),
    )
    .map_err(|err| {
        ScreenRecorderError::Decode(format!("failed to create audio resampler: {err}"))
    })?;

    let mut decoded = ffmpeg::frame::Audio::empty();
    let mut converted = ffmpeg::frame::Audio::empty();
    let mut out = Vec::<i16>::new();

    let mut drain_decoder = |decoder: &mut ffmpeg::decoder::Audio,
                             converted: &mut ffmpeg::frame::Audio,
                             out: &mut Vec<i16>|
     -> Result<()> {
        loop {
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    resampler.run(&decoded, converted).map_err(|err| {
                        ScreenRecorderError::Decode(format!(
                            "failed to resample decoded audio frame: {err}"
                        ))
                    })?;
                    append_audio_frame_i16_samples(converted, target_channels, out)?;
                }
                Err(err) if err == ffmpeg::Error::Eof => break,
                Err(err) if is_eagain(&err) => break,
                Err(err) => {
                    return Err(ScreenRecorderError::Decode(format!(
                        "failed to decode audio frame: {err}"
                    )));
                }
            }
        }
        Ok(())
    };

    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(|err| {
            ScreenRecorderError::Decode(format!("failed to send packet to audio decoder: {err}"))
        })?;
        drain_decoder(&mut decoder, &mut converted, &mut out)?;
    }

    decoder.send_eof().map_err(|err| {
        ScreenRecorderError::Decode(format!("failed to flush audio decoder: {err}"))
    })?;
    drain_decoder(&mut decoder, &mut converted, &mut out)?;

    loop {
        let mut flushed = ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
            DEFAULT_AUDIO_FRAME_SAMPLES,
            output_layout,
        );
        flushed.set_rate(target_rate_hz.max(1));
        let delay = match resampler.flush(&mut flushed) {
            Ok(delay) => delay,
            Err(ffmpeg::Error::OutputChanged) => break,
            Err(err) => {
                return Err(ScreenRecorderError::Decode(format!(
                    "failed to flush audio resampler: {err}"
                )));
            }
        };

        append_audio_frame_i16_samples(&flushed, target_channels, &mut out)?;
        if !delay.is_some_and(|d| d.output > 0) {
            break;
        }
    }

    Ok(out)
}

fn retime_audio_i16_interleaved(samples: &[i16], channels: u16, playback_speed: f32) -> Vec<i16> {
    let channels_usize = usize::from(channels.max(1));
    if channels_usize == 0 || samples.is_empty() {
        return Vec::new();
    }

    let frame_count = samples.len() / channels_usize;
    if frame_count == 0 {
        return Vec::new();
    }

    let speed = playback_speed.clamp(0.25, 4.0);
    if (speed - 1.0).abs() < f32::EPSILON {
        return samples.to_vec();
    }

    let output_frames = ((frame_count as f64) / speed as f64).ceil().max(1.0) as usize;
    let mut out = Vec::<i16>::with_capacity(output_frames * channels_usize);

    for out_frame in 0..output_frames {
        let src_pos = out_frame as f64 * speed as f64;
        let src_index = src_pos.floor() as usize;
        let next_index = (src_index + 1).min(frame_count.saturating_sub(1));
        let frac = (src_pos - src_index as f64) as f32;

        for ch in 0..channels_usize {
            let a = samples[src_index * channels_usize + ch] as f32;
            let b = samples[next_index * channels_usize + ch] as f32;
            let mixed = a + (b - a) * frac;
            out.push(mixed.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
        }
    }

    out
}

fn ms_to_audio_frames_round(ms: u64, sample_rate_hz: u32) -> usize {
    let scaled = u128::from(ms).saturating_mul(u128::from(sample_rate_hz.max(1)));
    let rounded = scaled.saturating_add(500) / 1000;
    rounded.min(usize::MAX as u128) as usize
}

fn trim_audio_i16_interleaved(
    samples: &[i16],
    sample_rate_hz: u32,
    channels: u16,
    trim_start_ms: u64,
    trim_duration_ms: Option<u64>,
) -> Vec<i16> {
    let channels_usize = usize::from(channels.max(1));
    if samples.is_empty() {
        return Vec::new();
    }

    let total_frames = samples.len() / channels_usize;
    if total_frames == 0 {
        return Vec::new();
    }

    let start_frame = ms_to_audio_frames_round(trim_start_ms, sample_rate_hz).min(total_frames);
    let end_frame = match trim_duration_ms {
        Some(duration_ms) => {
            let span_frames = ms_to_audio_frames_round(duration_ms, sample_rate_hz);
            start_frame.saturating_add(span_frames).min(total_frames)
        }
        None => total_frames,
    };
    if end_frame <= start_frame {
        return Vec::new();
    }

    let start_index = start_frame.saturating_mul(channels_usize);
    let end_index = end_frame.saturating_mul(channels_usize);
    samples[start_index..end_index].to_vec()
}

fn target_audio_frames_for_video_duration(
    video_frame_count: usize,
    export_fps: u32,
    sample_rate_hz: u32,
) -> usize {
    let scaled = (video_frame_count as u128).saturating_mul(u128::from(sample_rate_hz.max(1)));
    let fps = u128::from(export_fps.max(1));
    let rounded = scaled.saturating_add(fps / 2) / fps;
    rounded.min(usize::MAX as u128) as usize
}

fn align_mixed_audio_to_video_timeline(
    mut mixed: MixedPcmAudio,
    video_frame_count: usize,
    export_fps: u32,
) -> MixedPcmAudio {
    let target_frames =
        target_audio_frames_for_video_duration(video_frame_count, export_fps, mixed.sample_rate_hz);
    let target_samples = target_frames.saturating_mul(usize::from(mixed.channels.max(1)));

    match mixed.samples_i16.len().cmp(&target_samples) {
        std::cmp::Ordering::Greater => mixed.samples_i16.truncate(target_samples),
        std::cmp::Ordering::Less => mixed.samples_i16.resize(target_samples, 0),
        std::cmp::Ordering::Equal => {}
    }

    mixed
}

fn mix_audio_tracks_i16_interleaved(tracks: &[(Vec<i16>, f32)], channels: u16) -> Vec<i16> {
    let channels_usize = usize::from(channels.max(1));
    if channels_usize == 0 || tracks.is_empty() {
        return Vec::new();
    }

    let max_frames = tracks
        .iter()
        .map(|(samples, _)| samples.len() / channels_usize)
        .max()
        .unwrap_or(0);
    if max_frames == 0 {
        return Vec::new();
    }

    let mut mixed = vec![0i16; max_frames * channels_usize];
    for frame_idx in 0..max_frames {
        for ch in 0..channels_usize {
            let mut acc = 0.0f32;
            for (samples, volume) in tracks {
                let sample_index = frame_idx * channels_usize + ch;
                if sample_index < samples.len() {
                    acc += samples[sample_index] as f32 * *volume;
                }
            }
            mixed[frame_idx * channels_usize + ch] =
                acc.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }

    mixed
}

fn build_mixed_audio(audio: &Mp4AudioMixConfig) -> Result<Option<MixedPcmAudio>> {
    if audio.tracks.is_empty() {
        return Ok(None);
    }

    let mut prepared_tracks = Vec::<(Vec<i16>, f32)>::new();
    for track in &audio.tracks {
        let decoded =
            decode_audio_file_to_i16_samples(&track.path, audio.sample_rate_hz, audio.channels)?;
        let trimmed = trim_audio_i16_interleaved(
            &decoded,
            audio.sample_rate_hz,
            audio.channels,
            audio.trim_start_ms,
            audio.trim_duration_ms,
        );
        if trimmed.is_empty() {
            continue;
        }
        let retimed = retime_audio_i16_interleaved(&trimmed, audio.channels, audio.playback_speed);
        if retimed.is_empty() {
            continue;
        }
        prepared_tracks.push((retimed, track.volume.clamp(0.0, 2.0)));
    }

    if prepared_tracks.is_empty() {
        return Ok(None);
    }

    let mixed = mix_audio_tracks_i16_interleaved(&prepared_tracks, audio.channels);
    if mixed.is_empty() {
        return Ok(None);
    }

    Ok(Some(MixedPcmAudio {
        sample_rate_hz: audio.sample_rate_hz.max(1),
        channels: audio.channels.max(1),
        samples_i16: mixed,
    }))
}

fn drain_audio_packets(
    encoder: &mut ffmpeg::encoder::audio::Encoder,
    output: &mut ffmpeg::format::context::Output,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    draining: bool,
) -> Result<()> {
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                packet.set_stream(stream_index);
                packet.rescale_ts(encoder.time_base(), stream_time_base);
                packet.write_interleaved(output).map_err(|err| {
                    ScreenRecorderError::Export(format!(
                        "failed to write encoded audio packet: {err}"
                    ))
                })?;
            }
            Err(err) if err == ffmpeg::Error::Eof => break,
            Err(err) if is_eagain(&err) && !draining => break,
            Err(err) if is_eagain(&err) && draining => continue,
            Err(err) => {
                return Err(ScreenRecorderError::Export(format!(
                    "failed to receive encoded audio packet: {err}"
                )));
            }
        }
    }

    Ok(())
}

fn encode_mixed_audio_to_output(
    output: &mut ffmpeg::format::context::Output,
    encoder: &mut ffmpeg::encoder::audio::Encoder,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    mixed: &MixedPcmAudio,
) -> Result<()> {
    let input_layout = ffmpeg::ChannelLayout::default(i32::from(mixed.channels.max(1)));
    let mut resampler = ffmpeg::software::resampling::Context::get(
        ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
        input_layout,
        mixed.sample_rate_hz.max(1),
        encoder.format(),
        encoder.channel_layout(),
        encoder.rate(),
    )
    .map_err(|err| {
        ScreenRecorderError::Export(format!("failed to create encode resampler: {err}"))
    })?;

    let frame_samples = {
        let fs = encoder.frame_size() as usize;
        if fs == 0 {
            DEFAULT_AUDIO_FRAME_SAMPLES
        } else {
            fs
        }
    };
    let variable_frame_size = encoder.frame_size() == 0;
    let input_channels = usize::from(mixed.channels.max(1));

    let mut next_pts = 0i64;
    let mut sample_cursor = 0usize;
    let samples_per_chunk = if variable_frame_size {
        ((mixed.sample_rate_hz as usize / 25).max(1) * input_channels).max(input_channels)
    } else {
        frame_samples * input_channels
    };

    while sample_cursor < mixed.samples_i16.len() {
        let remaining = mixed.samples_i16.len() - sample_cursor;
        let take = remaining.min(samples_per_chunk);

        let mut chunk = mixed.samples_i16[sample_cursor..sample_cursor + take].to_vec();
        sample_cursor += take;

        if !variable_frame_size {
            let target_len = samples_per_chunk;
            if chunk.len() < target_len {
                chunk.resize(target_len, 0);
            }
        }

        if chunk.is_empty() {
            continue;
        }

        let input_samples_per_channel = chunk.len() / input_channels;
        let mut source = ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
            input_samples_per_channel,
            input_layout,
        );
        source.set_rate(mixed.sample_rate_hz.max(1));

        let expected_bytes = chunk.len() * 2;
        let source_data = source.data_mut(0);
        if source_data.len() < expected_bytes {
            return Err(ScreenRecorderError::Export(
                "allocated source audio frame is too small".to_string(),
            ));
        }

        for (i, sample) in chunk.iter().enumerate() {
            let bytes = sample.to_le_bytes();
            source_data[i * 2] = bytes[0];
            source_data[i * 2 + 1] = bytes[1];
        }

        let mut converted = ffmpeg::frame::Audio::empty();
        resampler.run(&source, &mut converted).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to resample audio for encoding: {err}"))
        })?;

        if converted.samples() == 0 {
            continue;
        }

        converted.set_pts(Some(next_pts));
        next_pts = next_pts.saturating_add(converted.samples() as i64);

        encoder.send_frame(&converted).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to send audio frame to encoder: {err}"))
        })?;
        drain_audio_packets(encoder, output, stream_index, stream_time_base, false)?;
    }

    loop {
        let mut flushed = ffmpeg::frame::Audio::new(
            encoder.format(),
            frame_samples.max(1),
            encoder.channel_layout(),
        );
        flushed.set_rate(encoder.rate());

        let delay = match resampler.flush(&mut flushed) {
            Ok(delay) => delay,
            Err(ffmpeg::Error::OutputChanged) => break,
            Err(err) => {
                return Err(ScreenRecorderError::Export(format!(
                    "failed to flush encode resampler: {err}"
                )));
            }
        };

        if flushed.samples() > 0 {
            flushed.set_pts(Some(next_pts));
            next_pts = next_pts.saturating_add(flushed.samples() as i64);
            encoder.send_frame(&flushed).map_err(|err| {
                ScreenRecorderError::Export(format!(
                    "failed to send flushed audio frame to encoder: {err}"
                ))
            })?;
            drain_audio_packets(encoder, output, stream_index, stream_time_base, false)?;
        }

        if !delay.is_some_and(|d| d.output > 0) {
            break;
        }
    }

    encoder.send_eof().map_err(|err| {
        ScreenRecorderError::Export(format!("failed to finalize audio encoder: {err}"))
    })?;
    drain_audio_packets(encoder, output, stream_index, stream_time_base, true)?;
    Ok(())
}

fn validate_export_frames(frames: &[ReconstructedFrame]) -> Result<(u32, u32)> {
    if frames.is_empty() {
        return Err(ScreenRecorderError::Export(
            "video export requires at least one frame".to_string(),
        ));
    }

    let width = frames[0].width;
    let height = frames[0].height;
    if width == 0 || height == 0 {
        return Err(ScreenRecorderError::Export(
            "video export requires non-zero frame dimensions".to_string(),
        ));
    }

    let expected_rgba_len = width as usize * height as usize * 4;
    for frame in frames {
        if frame.width != width || frame.height != height {
            return Err(ScreenRecorderError::Export(format!(
                "dynamic resolution is unsupported (expected {}x{}, got {}x{})",
                width, height, frame.width, frame.height
            )));
        }
        if frame.rgba.len() != expected_rgba_len {
            return Err(ScreenRecorderError::Export(
                "RGBA input size mismatch".to_string(),
            ));
        }
    }

    Ok((width, height))
}

fn resolve_mixed_audio(audio_mix: Option<&Mp4AudioMixConfig>) -> Result<Option<MixedPcmAudio>> {
    match audio_mix {
        Some(config) => build_mixed_audio(config),
        None => Ok(None),
    }
}

struct AudioEncoderState {
    encoder: ffmpeg::encoder::audio::Encoder,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    mixed: MixedPcmAudio,
}

struct Mp4Muxer {
    output: ffmpeg::format::context::Output,
    video_encoder: ffmpeg::encoder::video::Encoder,
    video_stream_index: usize,
    video_stream_time_base: ffmpeg::Rational,
    scaler: ffmpeg::software::scaling::Context,
    rgba_frame: ffmpeg::frame::Video,
    encode_frame: ffmpeg::frame::Video,
    width: u32,
    audio_state: Option<AudioEncoderState>,
}

impl Mp4Muxer {
    fn new(
        output_path: &Path,
        width: u32,
        height: u32,
        export_fps: u32,
        mixed_audio: Option<MixedPcmAudio>,
        requested_audio_bitrate_kbps: u16,
    ) -> Result<Self> {
        let video_codec = choose_h264_encoder().ok_or_else(|| {
            ScreenRecorderError::Export("no H264 encoder available in ffmpeg".to_string())
        })?;
        let pixel_format = choose_pixel_format(video_codec);
        if matches!(pixel_format, ffmpeg::format::Pixel::YUV420P)
            && (width % 2 != 0 || height % 2 != 0)
        {
            return Err(ScreenRecorderError::Export(
                "selected H264 pixel format requires even width/height".to_string(),
            ));
        }

        let mut output = ffmpeg::format::output(output_path).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to create ffmpeg output context: {err}"))
        })?;
        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);

        let fps = export_fps.max(1).min(i32::MAX as u32) as i32;
        let video_time_base = ffmpeg::Rational(1, fps);
        let video_frame_rate = ffmpeg::Rational(fps, 1);
        let gop_size = fps.max(1) as u32;

        let video_encoder = open_h264_encoder(
            video_codec,
            width,
            height,
            pixel_format,
            video_time_base,
            video_frame_rate,
            global_header,
            gop_size,
        )
        .map_err(|err| ScreenRecorderError::Export(err.to_string()))?;

        let video_stream_index = {
            let mut stream = output.add_stream(video_codec).map_err(|err| {
                ScreenRecorderError::Export(format!("failed to add output video stream: {err}"))
            })?;
            stream.set_time_base(video_time_base);
            stream.set_rate(video_frame_rate);
            stream.set_avg_frame_rate(video_frame_rate);
            stream.set_parameters(&video_encoder);
            stream.index()
        };

        let mut audio_state = None::<AudioEncoderState>;
        if let Some(mixed) = mixed_audio {
            let audio_codec_id = output
                .format()
                .codec(output_path, ffmpeg::media::Type::Audio);
            let audio_codec = ffmpeg::encoder::find(audio_codec_id).ok_or_else(|| {
                ScreenRecorderError::Export(format!(
                    "no audio encoder available for {:?}",
                    audio_codec_id
                ))
            })?;
            let audio_codec_info = audio_codec.audio().map_err(|err| {
                ScreenRecorderError::Export(format!("failed to use audio codec: {err}"))
            })?;

            let output_rate_hz = choose_audio_sample_rate(audio_codec_info, mixed.sample_rate_hz);
            let output_layout = choose_audio_channel_layout(audio_codec_info, mixed.channels);
            let output_sample_format = choose_audio_sample_format(audio_codec_info);
            let mut audio_encoder = ffmpeg::codec::context::Context::new_with_codec(audio_codec)
                .encoder()
                .audio()
                .map_err(|err| {
                    ScreenRecorderError::Export(format!(
                        "failed to create audio encoder context: {err}"
                    ))
                })?;
            audio_encoder.set_rate(output_rate_hz as i32);
            audio_encoder.set_channel_layout(output_layout);
            audio_encoder.set_format(output_sample_format);
            audio_encoder.set_bit_rate(usize::from(requested_audio_bitrate_kbps.max(8)) * 1000);
            audio_encoder.set_time_base((1, output_rate_hz as i32));

            if global_header {
                audio_encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
            }

            let mut options = ffmpeg::Dictionary::new();
            if audio_codec.name().eq_ignore_ascii_case("aac") {
                options.set("profile", "aac_low");
            }

            let audio_encoder = if options.iter().next().is_some() {
                audio_encoder
                    .open_as_with(audio_codec, options)
                    .map_err(|err| {
                        ScreenRecorderError::Export(format!("failed to open audio encoder: {err}"))
                    })?
            } else {
                audio_encoder.open_as(audio_codec).map_err(|err| {
                    ScreenRecorderError::Export(format!("failed to open audio encoder: {err}"))
                })?
            };

            let audio_time_base = ffmpeg::Rational(1, output_rate_hz as i32);
            let audio_stream_index = {
                let mut stream = output.add_stream(audio_codec).map_err(|err| {
                    ScreenRecorderError::Export(format!("failed to add output audio stream: {err}"))
                })?;
                stream.set_time_base(audio_time_base);
                stream.set_rate(ffmpeg::Rational(output_rate_hz as i32, 1));
                stream.set_parameters(&audio_encoder);
                stream.index()
            };

            audio_state = Some(AudioEncoderState {
                encoder: audio_encoder,
                stream_index: audio_stream_index,
                stream_time_base: audio_time_base,
                mixed,
            });
        }

        output.write_header().map_err(|err| {
            ScreenRecorderError::Export(format!("failed to write output header: {err}"))
        })?;

        let video_stream_time_base = output
            .stream(video_stream_index)
            .map(|stream| stream.time_base())
            .ok_or_else(|| {
                ScreenRecorderError::Export(format!(
                    "failed to resolve output video stream {} after header",
                    video_stream_index
                ))
            })?;
        if let Some(state) = audio_state.as_mut() {
            state.stream_time_base = output
                .stream(state.stream_index)
                .map(|stream| stream.time_base())
                .ok_or_else(|| {
                    ScreenRecorderError::Export(format!(
                        "failed to resolve output audio stream {} after header",
                        state.stream_index
                    ))
                })?;
        }

        let scaler = ffmpeg::software::scaling::Context::get(
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            pixel_format,
            width,
            height,
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )
        .map_err(|err| {
            ScreenRecorderError::Export(format!("failed to create RGBA video scaler: {err}"))
        })?;

        Ok(Self {
            output,
            video_encoder,
            video_stream_index,
            video_stream_time_base,
            scaler,
            rgba_frame: ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, width, height),
            encode_frame: ffmpeg::frame::Video::new(pixel_format, width, height),
            width,
            audio_state,
        })
    }

    fn encode_video_frames(&mut self, frames: &[ReconstructedFrame]) -> Result<()> {
        for (index, frame) in frames.iter().enumerate() {
            copy_rgba_into_frame(&mut self.rgba_frame, self.width, &frame.rgba);
            self.scaler
                .run(&self.rgba_frame, &mut self.encode_frame)
                .map_err(|err| {
                    ScreenRecorderError::Export(format!(
                        "failed to convert frame for video export: {err}"
                    ))
                })?;
            self.encode_frame.set_pts(Some(index as i64));

            self.video_encoder
                .send_frame(&self.encode_frame)
                .map_err(|err| {
                    ScreenRecorderError::Export(format!(
                        "failed to send frame to video encoder: {err}"
                    ))
                })?;
            drain_export_packets(
                &mut self.video_encoder,
                &mut self.output,
                self.video_stream_index,
                self.video_stream_time_base,
                false,
            )?;
        }
        Ok(())
    }

    fn finalize_video(&mut self) -> Result<()> {
        self.video_encoder.send_eof().map_err(|err| {
            ScreenRecorderError::Export(format!("failed to finalize video encoder: {err}"))
        })?;
        drain_export_packets(
            &mut self.video_encoder,
            &mut self.output,
            self.video_stream_index,
            self.video_stream_time_base,
            true,
        )
    }

    fn encode_audio_if_present(&mut self) -> Result<()> {
        let Some(mut audio_state) = self.audio_state.take() else {
            return Ok(());
        };

        encode_mixed_audio_to_output(
            &mut self.output,
            &mut audio_state.encoder,
            audio_state.stream_index,
            audio_state.stream_time_base,
            &audio_state.mixed,
        )
    }

    fn finish(mut self) -> Result<()> {
        self.output.write_trailer().map_err(|err| {
            ScreenRecorderError::Export(format!("failed to write output trailer: {err}"))
        })?;
        Ok(())
    }
}

pub fn export_frames_to_mp4_with_audio(
    output_path: &Path,
    frames: &[ReconstructedFrame],
    export_fps: u32,
    audio_mix: Option<&Mp4AudioMixConfig>,
) -> Result<()> {
    ensure_ffmpeg_encode()?;
    let (width, height) = validate_export_frames(frames)?;
    let mixed_audio = resolve_mixed_audio(audio_mix)?
        .map(|mixed| align_mixed_audio_to_video_timeline(mixed, frames.len(), export_fps));
    let requested_audio_bitrate_kbps = audio_mix
        .map(|cfg| cfg.bitrate_kbps)
        .unwrap_or(DEFAULT_AUDIO_BITRATE_KBPS);

    let mut muxer = Mp4Muxer::new(
        output_path,
        width,
        height,
        export_fps,
        mixed_audio,
        requested_audio_bitrate_kbps,
    )?;
    muxer.encode_video_frames(frames)?;
    muxer.finalize_video()?;
    muxer.encode_audio_if_present()?;
    muxer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_audio_applies_start_and_duration_window() {
        let samples: Vec<i16> = (0..20).collect();
        let trimmed = trim_audio_i16_interleaved(&samples, 10, 1, 500, Some(1_000));
        assert_eq!(trimmed, (5..15).collect::<Vec<i16>>());
    }

    #[test]
    fn align_audio_pads_or_truncates_to_match_video_duration() {
        let padded = align_mixed_audio_to_video_timeline(
            MixedPcmAudio {
                sample_rate_hz: 10,
                channels: 2,
                samples_i16: vec![1; 20],
            },
            3,
            2,
        );
        assert_eq!(padded.samples_i16.len(), 30);
        assert!(padded.samples_i16[20..].iter().all(|sample| *sample == 0));

        let truncated = align_mixed_audio_to_video_timeline(
            MixedPcmAudio {
                sample_rate_hz: 10,
                channels: 2,
                samples_i16: vec![1; 40],
            },
            3,
            2,
        );
        assert_eq!(truncated.samples_i16.len(), 30);
    }
}
