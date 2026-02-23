use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ffmpeg_next as ffmpeg;

use super::frame_cache::ReconstructedFrame;
use crate::error::{Result, ScreenRecorderError};

#[path = "encoder_h264_lossless/mp4_muxer.rs"]
mod mp4_muxer;
pub use mp4_muxer::{AudioMixTrack, Mp4AudioMixConfig, export_frames_to_mp4_with_audio};

const DEFAULT_FPS: i32 = 60;
const DEFAULT_FRAME_DURATION_MS: u32 = 16;

fn ensure_ffmpeg_initialized() -> std::result::Result<(), String> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| ffmpeg::init().map_err(|err| err.to_string()))
        .clone()
}

fn ensure_ffmpeg_encode() -> Result<()> {
    ensure_ffmpeg_initialized()
        .map_err(|err| ScreenRecorderError::Encode(format!("failed to initialize ffmpeg: {err}")))
}

fn ensure_ffmpeg_decode() -> Result<()> {
    ensure_ffmpeg_initialized()
        .map_err(|err| ScreenRecorderError::Decode(format!("failed to initialize ffmpeg: {err}")))
}

fn validate_rgba_input(width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(ScreenRecorderError::Encode(
            "H264 encoder requires non-zero dimensions".to_string(),
        ));
    }

    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(ScreenRecorderError::Encode(
            "RGBA input size mismatch".to_string(),
        ));
    }

    Ok(())
}

fn is_eagain(err: &ffmpeg::Error) -> bool {
    matches!(
        err,
        ffmpeg::Error::Other { errno } if *errno == ffmpeg::error::EAGAIN
    )
}

fn copy_rgba_into_frame(frame: &mut ffmpeg::frame::Video, width: u32, rgba: &[u8]) {
    let src_row_bytes = width as usize * 4;
    let dst_stride = frame.stride(0);
    let dst = frame.data_mut(0);
    for (row, src_row) in rgba.chunks_exact(src_row_bytes).enumerate() {
        let dst_start = row * dst_stride;
        let dst_end = dst_start + src_row_bytes;
        dst[dst_start..dst_end].copy_from_slice(src_row);
    }
}

fn copy_frame_to_rgba(frame: &ffmpeg::frame::Video) -> Vec<u8> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let row_bytes = width * 4;
    let src_stride = frame.stride(0);
    let src = frame.data(0);

    let mut out = vec![0u8; row_bytes * height];
    for row in 0..height {
        let src_start = row * src_stride;
        let src_end = src_start + row_bytes;
        let dst_start = row * row_bytes;
        let dst_end = dst_start + row_bytes;
        out[dst_start..dst_end].copy_from_slice(&src[src_start..src_end]);
    }
    out
}

fn choose_h264_encoder() -> Option<ffmpeg::Codec> {
    ffmpeg::encoder::find_by_name("libx264rgb")
        .or_else(|| ffmpeg::encoder::find_by_name("libx264"))
        .or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::H264))
}

fn choose_pixel_format(codec: ffmpeg::Codec) -> ffmpeg::format::Pixel {
    let preferred = [
        ffmpeg::format::Pixel::BGRA,
        ffmpeg::format::Pixel::RGBA,
        ffmpeg::format::Pixel::RGB24,
        ffmpeg::format::Pixel::BGR24,
        ffmpeg::format::Pixel::YUV444P,
        ffmpeg::format::Pixel::YUV420P,
    ];

    if let Ok(video_codec) = codec.video() {
        if let Some(formats) = video_codec.formats() {
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
    }

    ffmpeg::format::Pixel::YUV420P
}

fn configure_video_encoder(
    codec: ffmpeg::Codec,
    width: u32,
    height: u32,
    pixel_format: ffmpeg::format::Pixel,
    time_base: ffmpeg::Rational,
    frame_rate: ffmpeg::Rational,
    global_header: bool,
    gop_size: u32,
) -> Result<ffmpeg::codec::encoder::video::Video> {
    let mut video = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to create video encoder context: {err}"))
        })?;

    video.set_width(width);
    video.set_height(height);
    video.set_format(pixel_format);
    video.set_time_base(time_base);
    video.set_frame_rate(Some(frame_rate));
    video.set_gop(gop_size);
    video.set_max_b_frames(0);

    if global_header {
        video.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
    }

    Ok(video)
}

fn open_h264_encoder(
    codec: ffmpeg::Codec,
    width: u32,
    height: u32,
    pixel_format: ffmpeg::format::Pixel,
    time_base: ffmpeg::Rational,
    frame_rate: ffmpeg::Rational,
    global_header: bool,
    gop_size: u32,
) -> Result<ffmpeg::encoder::video::Encoder> {
    let codec_name = codec.name().to_ascii_lowercase();
    if codec_name.contains("libx264") {
        let mut options = ffmpeg::Dictionary::new();
        options.set("preset", "ultrafast");
        options.set("tune", "zerolatency");
        options.set("crf", "0");
        options.set("x264-params", "repeat-headers=1:annexb=1");

        let configured = configure_video_encoder(
            codec,
            width,
            height,
            pixel_format,
            time_base,
            frame_rate,
            global_header,
            gop_size,
        )?;
        if let Ok(opened) = configured.open_as_with(codec, options) {
            return Ok(opened);
        }
    }

    configure_video_encoder(
        codec,
        width,
        height,
        pixel_format,
        time_base,
        frame_rate,
        global_header,
        gop_size,
    )?
    .open_as(codec)
    .map_err(|err| ScreenRecorderError::Encode(format!("failed to open H264 encoder: {err}")))
}

fn choose_supported_pixel_format(
    codec: ffmpeg::Codec,
    preferred: &[ffmpeg::format::Pixel],
    fallback: ffmpeg::format::Pixel,
) -> ffmpeg::format::Pixel {
    if let Ok(video_codec) = codec.video() {
        if let Some(formats) = video_codec.formats() {
            let available: Vec<_> = formats.collect();
            for candidate in preferred {
                if available.iter().any(|fmt| *fmt == *candidate) {
                    return *candidate;
                }
            }
            if let Some(first) = available.first().copied() {
                return first;
            }
        }
    }

    fallback
}

#[derive(Clone, Copy, Debug)]
enum ExportCodecProfile {
    Mjpeg,
    Gif,
}

fn choose_export_codec(profile: ExportCodecProfile) -> Option<ffmpeg::Codec> {
    match profile {
        ExportCodecProfile::Mjpeg => ffmpeg::encoder::find(ffmpeg::codec::Id::MJPEG),
        ExportCodecProfile::Gif => ffmpeg::encoder::find(ffmpeg::codec::Id::GIF),
    }
}

fn choose_export_pixel_format(
    codec: ffmpeg::Codec,
    profile: ExportCodecProfile,
) -> ffmpeg::format::Pixel {
    match profile {
        ExportCodecProfile::Mjpeg => choose_supported_pixel_format(
            codec,
            &[
                ffmpeg::format::Pixel::YUVJ444P,
                ffmpeg::format::Pixel::YUVJ422P,
                ffmpeg::format::Pixel::YUVJ420P,
                ffmpeg::format::Pixel::YUV444P,
                ffmpeg::format::Pixel::YUV422P,
                ffmpeg::format::Pixel::YUV420P,
                ffmpeg::format::Pixel::RGB24,
            ],
            ffmpeg::format::Pixel::YUVJ420P,
        ),
        ExportCodecProfile::Gif => choose_supported_pixel_format(
            codec,
            &[
                ffmpeg::format::Pixel::PAL8,
                ffmpeg::format::Pixel::RGB8,
                ffmpeg::format::Pixel::RGB24,
                ffmpeg::format::Pixel::BGR24,
            ],
            ffmpeg::format::Pixel::PAL8,
        ),
    }
}

fn open_export_encoder(
    codec: ffmpeg::Codec,
    profile: ExportCodecProfile,
    width: u32,
    height: u32,
    pixel_format: ffmpeg::format::Pixel,
    time_base: ffmpeg::Rational,
    frame_rate: ffmpeg::Rational,
    global_header: bool,
    gop_size: u32,
) -> Result<ffmpeg::encoder::video::Encoder> {
    match profile {
        ExportCodecProfile::Mjpeg | ExportCodecProfile::Gif => configure_video_encoder(
            codec,
            width,
            height,
            pixel_format,
            time_base,
            frame_rate,
            global_header,
            gop_size,
        )?
        .open_as(codec)
        .map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to open {} encoder: {err}", codec.name()))
        }),
    }
}

fn timestamp_to_ms(value: i64, time_base: ffmpeg::Rational) -> Option<u64> {
    let num = i128::from(time_base.numerator());
    let den = i128::from(time_base.denominator());
    if den == 0 {
        return None;
    }

    let scaled = i128::from(value).saturating_mul(num).saturating_mul(1000);
    if scaled < 0 {
        return None;
    }

    Some((scaled / den) as u64)
}

fn frame_duration_from_rate(rate: ffmpeg::Rational) -> u32 {
    let num = rate.numerator();
    let den = rate.denominator();
    if num <= 0 || den <= 0 {
        return DEFAULT_FRAME_DURATION_MS;
    }
    let duration = ((1000.0 * den as f64) / num as f64).round();
    duration.clamp(1.0, 1000.0) as u32
}

fn drain_export_packets(
    encoder: &mut ffmpeg::encoder::video::Encoder,
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
                        "failed to write encoded video packet: {err}"
                    ))
                })?;
            }
            Err(err) if err == ffmpeg::Error::Eof => break,
            Err(err) if is_eagain(&err) && !draining => break,
            Err(err) if is_eagain(&err) && draining => continue,
            Err(err) => {
                return Err(ScreenRecorderError::Export(format!(
                    "failed to receive encoded packet: {err}"
                )));
            }
        }
    }

    Ok(())
}

struct FileWriterInner {
    output: ffmpeg::format::context::Output,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    encoder: ffmpeg::encoder::video::Encoder,
    scaler: ffmpeg::software::scaling::Context,
    rgba_frame: ffmpeg::frame::Video,
    encode_frame: ffmpeg::frame::Video,
    width: u32,
    height: u32,
    next_pts: i64,
}

impl FileWriterInner {
    fn new(path: &Path, width: u32, height: u32) -> Result<Self> {
        let codec = choose_h264_encoder().ok_or_else(|| {
            ScreenRecorderError::Encode("no H264 encoder available in ffmpeg".to_string())
        })?;
        let pixel_format = choose_pixel_format(codec);
        if pixel_format == ffmpeg::format::Pixel::YUV420P && (width % 2 != 0 || height % 2 != 0) {
            return Err(ScreenRecorderError::Encode(
                "selected H264 pixel format requires even width/height".to_string(),
            ));
        }

        let mut output = ffmpeg::format::output(path).map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to create ffmpeg output context: {err}"))
        })?;

        let time_base = ffmpeg::Rational(1, DEFAULT_FPS);
        let frame_rate = ffmpeg::Rational(DEFAULT_FPS, 1);
        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);

        let encoder = open_h264_encoder(
            codec,
            width,
            height,
            pixel_format,
            time_base,
            frame_rate,
            global_header,
            DEFAULT_FPS as u32,
        )?;

        let stream_index = {
            let mut stream = output.add_stream(codec).map_err(|err| {
                ScreenRecorderError::Encode(format!("failed to add output video stream: {err}"))
            })?;
            stream.set_time_base(time_base);
            stream.set_rate(frame_rate);
            stream.set_avg_frame_rate(frame_rate);
            stream.set_parameters(&encoder);
            stream.index()
        };

        output.write_header().map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to write output header: {err}"))
        })?;
        let stream_time_base = output
            .stream(stream_index)
            .map(|stream| stream.time_base())
            .ok_or_else(|| {
                ScreenRecorderError::Encode(format!(
                    "failed to resolve output video stream {} after header",
                    stream_index
                ))
            })?;

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
            ScreenRecorderError::Encode(format!("failed to create RGBA->H264 scaler: {err}"))
        })?;

        Ok(Self {
            output,
            stream_index,
            stream_time_base,
            encoder,
            scaler,
            rgba_frame: ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, width, height),
            encode_frame: ffmpeg::frame::Video::new(pixel_format, width, height),
            width,
            height,
            next_pts: 0,
        })
    }

    fn write_rgba_frame(&mut self, rgba: &[u8]) -> Result<()> {
        copy_rgba_into_frame(&mut self.rgba_frame, self.width, rgba);
        self.scaler
            .run(&self.rgba_frame, &mut self.encode_frame)
            .map_err(|err| {
                ScreenRecorderError::Encode(format!(
                    "failed to convert frame format for H264: {err}"
                ))
            })?;

        self.encode_frame.set_pts(Some(self.next_pts));
        self.next_pts += 1;

        self.encoder.send_frame(&self.encode_frame).map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to send frame to H264 encoder: {err}"))
        })?;

        self.drain_packets(false)
    }

    fn drain_packets(&mut self, draining: bool) -> Result<()> {
        loop {
            let mut packet = ffmpeg::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.stream_index);
                    packet.rescale_ts(self.encoder.time_base(), self.stream_time_base);
                    packet.write_interleaved(&mut self.output).map_err(|err| {
                        ScreenRecorderError::Encode(format!(
                            "failed to write encoded video packet: {err}"
                        ))
                    })?;
                }
                Err(err) if err == ffmpeg::Error::Eof => break,
                Err(err) if is_eagain(&err) && !draining => break,
                Err(err) if is_eagain(&err) && draining => continue,
                Err(err) => {
                    return Err(ScreenRecorderError::Encode(format!(
                        "failed to receive encoded packet: {err}"
                    )));
                }
            }
        }

        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.encoder.send_eof().map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to finalize H264 encoder: {err}"))
        })?;
        self.drain_packets(true)?;
        self.output.write_trailer().map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to write output trailer: {err}"))
        })?;
        Ok(())
    }
}

pub struct H264LosslessFileWriter {
    path: PathBuf,
    inner: Option<FileWriterInner>,
}

impl H264LosslessFileWriter {
    pub fn create(path: &Path) -> Result<Self> {
        ensure_ffmpeg_encode()?;
        File::create(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            inner: None,
        })
    }

    pub fn write_rgba_frame(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        validate_rgba_input(width, height, rgba)?;

        if self.inner.is_none() {
            self.inner = Some(FileWriterInner::new(&self.path, width, height)?);
        }

        let inner = self.inner.as_mut().ok_or_else(|| {
            ScreenRecorderError::Encode("video writer not initialized".to_string())
        })?;

        if inner.width != width || inner.height != height {
            return Err(ScreenRecorderError::Encode(format!(
                "dynamic resolution is unsupported (expected {}x{}, got {}x{})",
                inner.width, inner.height, width, height
            )));
        }

        inner.write_rgba_frame(rgba)
    }

    pub fn flush(self) -> Result<()> {
        if let Some(inner) = self.inner {
            inner.finish()?;
        }
        Ok(())
    }
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

fn export_frames_with_profile(
    output_path: &Path,
    frames: &[ReconstructedFrame],
    export_fps: u32,
    profile: ExportCodecProfile,
) -> Result<()> {
    ensure_ffmpeg_encode()?;
    let (width, height) = validate_export_frames(frames)?;

    let codec = choose_export_codec(profile).ok_or_else(|| {
        let codec_name = match profile {
            ExportCodecProfile::Mjpeg => "MJPEG",
            ExportCodecProfile::Gif => "GIF",
        };
        ScreenRecorderError::Export(format!("no {codec_name} encoder available in ffmpeg"))
    })?;
    let pixel_format = choose_export_pixel_format(codec, profile);
    if matches!(
        pixel_format,
        ffmpeg::format::Pixel::YUV420P | ffmpeg::format::Pixel::YUVJ420P
    ) && (width % 2 != 0 || height % 2 != 0)
    {
        return Err(ScreenRecorderError::Export(
            "selected pixel format requires even width/height".to_string(),
        ));
    }

    let mut output = ffmpeg::format::output(output_path).map_err(|err| {
        ScreenRecorderError::Export(format!("failed to create ffmpeg output context: {err}"))
    })?;

    let fps = export_fps.max(1).min(i32::MAX as u32) as i32;
    let time_base = ffmpeg::Rational(1, fps);
    let frame_rate = ffmpeg::Rational(fps, 1);
    let gop_size = fps.max(1) as u32;
    let global_header = output
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER);

    let mut encoder = open_export_encoder(
        codec,
        profile,
        width,
        height,
        pixel_format,
        time_base,
        frame_rate,
        global_header,
        gop_size,
    )
    .map_err(|err| ScreenRecorderError::Export(err.to_string()))?;

    let stream_index = {
        let mut stream = output.add_stream(codec).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to add output video stream: {err}"))
        })?;
        stream.set_time_base(time_base);
        stream.set_rate(frame_rate);
        stream.set_avg_frame_rate(frame_rate);
        stream.set_parameters(&encoder);
        stream.index()
    };

    output.write_header().map_err(|err| {
        ScreenRecorderError::Export(format!("failed to write output header: {err}"))
    })?;
    let stream_time_base = output
        .stream(stream_index)
        .map(|stream| stream.time_base())
        .ok_or_else(|| {
            ScreenRecorderError::Export(format!(
                "failed to resolve output video stream {} after header",
                stream_index
            ))
        })?;

    let mut scaler = ffmpeg::software::scaling::Context::get(
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

    let mut rgba_frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, width, height);
    let mut encode_frame = ffmpeg::frame::Video::new(pixel_format, width, height);

    for (index, frame) in frames.iter().enumerate() {
        copy_rgba_into_frame(&mut rgba_frame, width, &frame.rgba);
        scaler.run(&rgba_frame, &mut encode_frame).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to convert frame for video export: {err}"))
        })?;
        encode_frame.set_pts(Some(index as i64));

        encoder.send_frame(&encode_frame).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to send frame to video encoder: {err}"))
        })?;
        drain_export_packets(
            &mut encoder,
            &mut output,
            stream_index,
            stream_time_base,
            false,
        )?;
    }

    encoder.send_eof().map_err(|err| {
        ScreenRecorderError::Export(format!("failed to finalize video encoder: {err}"))
    })?;
    drain_export_packets(
        &mut encoder,
        &mut output,
        stream_index,
        stream_time_base,
        true,
    )?;

    output.write_trailer().map_err(|err| {
        ScreenRecorderError::Export(format!("failed to write output trailer: {err}"))
    })?;
    Ok(())
}

pub fn export_frames_to_avi(
    output_path: &Path,
    frames: &[ReconstructedFrame],
    export_fps: u32,
) -> Result<()> {
    export_frames_with_profile(output_path, frames, export_fps, ExportCodecProfile::Mjpeg)
}

pub fn export_frames_to_gif(
    output_path: &Path,
    frames: &[ReconstructedFrame],
    export_fps: u32,
) -> Result<()> {
    export_frames_with_profile(output_path, frames, export_fps, ExportCodecProfile::Gif)
}

#[derive(Clone, Debug)]
struct DecodedSample {
    timestamp_ms: u64,
    duration_ms: Option<u32>,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn drain_decoder_frames(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut ffmpeg::software::scaling::Context,
    decoded_frame: &mut ffmpeg::frame::Video,
    rgba_frame: &mut ffmpeg::frame::Video,
    stream_time_base: ffmpeg::Rational,
    fallback_duration_ms: u32,
    next_fallback_ts_ms: &mut u64,
    out: &mut Vec<DecodedSample>,
) -> Result<()> {
    loop {
        match decoder.receive_frame(decoded_frame) {
            Ok(()) => {
                let needs_rebuild = scaler.input().format != decoded_frame.format()
                    || scaler.input().width != decoded_frame.width()
                    || scaler.input().height != decoded_frame.height();
                if needs_rebuild {
                    scaler.cached(
                        decoded_frame.format(),
                        decoded_frame.width(),
                        decoded_frame.height(),
                        ffmpeg::format::Pixel::RGBA,
                        decoded_frame.width(),
                        decoded_frame.height(),
                        ffmpeg::software::scaling::flag::Flags::BILINEAR,
                    );
                    *rgba_frame = ffmpeg::frame::Video::new(
                        ffmpeg::format::Pixel::RGBA,
                        decoded_frame.width(),
                        decoded_frame.height(),
                    );
                }

                scaler.run(decoded_frame, rgba_frame).map_err(|err| {
                    ScreenRecorderError::Decode(format!(
                        "failed to convert decoded frame to RGBA: {err}"
                    ))
                })?;

                let mut timestamp_ms = decoded_frame
                    .timestamp()
                    .or_else(|| decoded_frame.pts())
                    .and_then(|value| timestamp_to_ms(value, stream_time_base))
                    .unwrap_or(*next_fallback_ts_ms);

                if let Some(prev) = out.last() {
                    let min_ts = prev.timestamp_ms.saturating_add(1);
                    if timestamp_ms < min_ts {
                        timestamp_ms = min_ts;
                    }
                }

                *next_fallback_ts_ms =
                    timestamp_ms.saturating_add(u64::from(fallback_duration_ms.max(1)));

                let packet_duration_ms = if decoded_frame.packet().duration > 0 {
                    timestamp_to_ms(decoded_frame.packet().duration, stream_time_base)
                        .map(|value| value.clamp(1, u64::from(u32::MAX)) as u32)
                } else {
                    None
                };

                out.push(DecodedSample {
                    timestamp_ms,
                    duration_ms: packet_duration_ms,
                    width: rgba_frame.width(),
                    height: rgba_frame.height(),
                    rgba: copy_frame_to_rgba(rgba_frame),
                });
            }
            Err(err) if err == ffmpeg::Error::Eof => break,
            Err(err) if is_eagain(&err) => break,
            Err(err) => {
                return Err(ScreenRecorderError::Decode(format!(
                    "failed to decode H264 frame: {err}"
                )));
            }
        }
    }

    Ok(())
}

pub fn decode_h264_video_to_frames(path: &Path) -> Result<Vec<ReconstructedFrame>> {
    ensure_ffmpeg_decode()?;

    let mut input = ffmpeg::format::input(path)
        .map_err(|err| ScreenRecorderError::Decode(format!("failed to open video file: {err}")))?;

    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| ScreenRecorderError::Decode("no video stream found".to_string()))?;
    let stream_index = stream.index();
    let stream_time_base = stream.time_base();
    let fallback_duration_ms = frame_duration_from_rate(stream.avg_frame_rate());

    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|err| {
            ScreenRecorderError::Decode(format!("failed to initialize H264 decoder context: {err}"))
        })?
        .decoder()
        .video()
        .map_err(|err| {
            ScreenRecorderError::Decode(format!("failed to open H264 decoder: {err}"))
        })?;

    let mut scaler = ffmpeg::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg::format::Pixel::RGBA,
        decoder.width(),
        decoder.height(),
        ffmpeg::software::scaling::flag::Flags::BILINEAR,
    )
    .map_err(|err| ScreenRecorderError::Decode(format!("failed to create decode scaler: {err}")))?;

    let mut decoded_frame = ffmpeg::frame::Video::empty();
    let mut rgba_frame = ffmpeg::frame::Video::new(
        ffmpeg::format::Pixel::RGBA,
        decoder.width(),
        decoder.height(),
    );
    let mut samples = Vec::<DecodedSample>::new();
    let mut next_fallback_ts_ms = 0u64;

    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(|err| {
            ScreenRecorderError::Decode(format!("failed to send packet to H264 decoder: {err}"))
        })?;
        drain_decoder_frames(
            &mut decoder,
            &mut scaler,
            &mut decoded_frame,
            &mut rgba_frame,
            stream_time_base,
            fallback_duration_ms,
            &mut next_fallback_ts_ms,
            &mut samples,
        )?;
    }

    decoder.send_eof().map_err(|err| {
        ScreenRecorderError::Decode(format!("failed to flush H264 decoder: {err}"))
    })?;
    drain_decoder_frames(
        &mut decoder,
        &mut scaler,
        &mut decoded_frame,
        &mut rgba_frame,
        stream_time_base,
        fallback_duration_ms,
        &mut next_fallback_ts_ms,
        &mut samples,
    )?;

    if samples.is_empty() {
        return Err(ScreenRecorderError::Decode(
            "video decode produced no frames".to_string(),
        ));
    }

    for idx in 0..samples.len() {
        let next_timestamp = samples.get(idx + 1).map(|next| next.timestamp_ms);
        let duration_ms = match next_timestamp {
            Some(next) => next.saturating_sub(samples[idx].timestamp_ms).max(1) as u32,
            None => samples[idx]
                .duration_ms
                .unwrap_or(fallback_duration_ms)
                .max(1),
        };
        samples[idx].duration_ms = Some(duration_ms);
    }

    Ok(samples
        .into_iter()
        .map(|sample| ReconstructedFrame {
            timestamp_ms: sample.timestamp_ms,
            duration_ms: sample.duration_ms.unwrap_or(fallback_duration_ms).max(1),
            width: sample.width,
            height: sample.height,
            rgba: sample.rgba,
        })
        .collect())
}
