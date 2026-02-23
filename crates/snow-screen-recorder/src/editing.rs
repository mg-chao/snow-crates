use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEncoder};
use mp4e::{Codec as Mp4Codec, Mp4e};

use crate::artifact::{PauseInterval, RecordingArtifact, SessionManifest};
use crate::audio::mixer::{Mp3SourceInput, encode_pcm_to_mp3, mix_mp3_sources_to_pcm};
use crate::audio::opus_encoder::OpusEncoderWrapper;
use crate::config::{EditConfig, ExportFormat, MouseEditConfig};
use crate::container::avi_writer::{AviAudioTrack, write_avi};
use crate::error::{Result, ScreenRecorderError};
use crate::export::ExportResult;
use crate::mouse::{ClickEventRecord, CursorSampleRecord, MouseRecord, read_mouse_records};
use crate::video::encoder_h264_lossless::H264LosslessAnnexBEncoder;
use crate::video::frame_cache::{ReconstructedFrame, reconstruct_frames};

pub struct EditingSession {
    artifact: RecordingArtifact,
    manifest: SessionManifest,
    config: EditConfig,
}

impl EditingSession {
    pub fn open(artifact: RecordingArtifact) -> Result<Self> {
        let manifest = artifact.load_manifest()?;
        validate_manifest_paths(&manifest)?;

        let mut config = EditConfig::default();
        config.playback_speed = 1.0;
        config.system_audio.enabled = manifest.recorded_system_audio;
        config.microphone_audio.enabled = manifest.recorded_microphone_audio;
        config.export.format = ExportFormat::Mp4;
        config.export.quality = 100;
        config.export.output_path = manifest
            .output_dir
            .join(format!("{}.mp4", manifest.session_id));

        Ok(Self {
            artifact,
            manifest,
            config,
        })
    }

    pub fn set_config(&mut self, config: EditConfig) -> Result<()> {
        config
            .validate()
            .map_err(ScreenRecorderError::InvalidConfig)?;

        if config.system_audio.enabled && !self.manifest.recorded_system_audio {
            return Err(ScreenRecorderError::InvalidConfig(
                "system audio cannot be enabled because it was not recorded".to_string(),
            ));
        }
        if config.microphone_audio.enabled && !self.manifest.recorded_microphone_audio {
            return Err(ScreenRecorderError::InvalidConfig(
                "microphone audio cannot be enabled because it was not recorded".to_string(),
            ));
        }

        self.config = config;
        Ok(())
    }

    pub fn export(self) -> Result<ExportResult> {
        self.config
            .validate()
            .map_err(ScreenRecorderError::InvalidConfig)?;

        let temp_dir = self.artifact.temp_dir.clone();
        let result = self.export_inner();
        if result.is_ok() {
            fs::remove_dir_all(&temp_dir).map_err(|err| {
                ScreenRecorderError::Export(format!(
                    "export succeeded but failed to cleanup temp directory {}: {err}",
                    temp_dir.display()
                ))
            })?;
        }
        result
    }

    fn export_inner(self) -> Result<ExportResult> {
        if let Some(parent) = self.config.export.output_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let source_frames = reconstruct_frames(&self.manifest.frame_cache_path)?;
        if source_frames.is_empty() {
            return Err(ScreenRecorderError::Export(
                "no frames available for export".to_string(),
            ));
        }

        let mouse_tracks = build_mouse_tracks(
            &self.manifest.pause_intervals,
            &read_mouse_records(&self.manifest.mouse_path)?,
        );

        let params = quality_params(
            self.manifest.fps,
            self.config.export.quality,
            self.config.export.format,
        );
        let mut frames = retime_frames(
            &source_frames,
            self.config.playback_speed,
            params.export_fps,
        )?;
        if frames.is_empty() {
            return Err(ScreenRecorderError::Export(
                "retiming produced no frames".to_string(),
            ));
        }

        let (scaled_w, scaled_h) = scaled_dimensions(
            source_frames[0].width,
            source_frames[0].height,
            params.scale,
            self.config.export.format == ExportFormat::Mp4,
        );

        for frame in &mut frames {
            apply_mouse_overlays(frame, &mouse_tracks, &self.config.mouse);
            if frame.width != scaled_w || frame.height != scaled_h {
                frame.rgba =
                    resize_rgba_nearest(&frame.rgba, frame.width, frame.height, scaled_w, scaled_h);
                frame.width = scaled_w;
                frame.height = scaled_h;
            }
        }

        match self.config.export.format {
            ExportFormat::Mp4 => export_mp4(
                &self.config.export.output_path,
                &frames,
                self.mix_audio_pcm(48_000, 2)?,
                self.manifest.audio_bitrate_kbps,
            )?,
            ExportFormat::Avi => export_avi(
                &self.config.export.output_path,
                &frames,
                params.export_fps,
                self.mix_audio_pcm(
                    self.manifest.audio_sample_rate_hz,
                    self.manifest.audio_channels,
                )?,
                self.manifest.audio_sample_rate_hz,
                self.manifest.audio_channels,
                self.manifest.audio_bitrate_kbps,
                self.config.export.quality,
            )?,
            ExportFormat::Gif => export_gif(
                &self.config.export.output_path,
                &frames,
                self.config.export.quality,
            )?,
        }

        let duration_ms = frames
            .iter()
            .map(|f| u64::from(f.duration_ms))
            .sum::<u64>()
            .max(1);

        Ok(ExportResult {
            output_path: self.config.export.output_path,
            duration_ms,
            format: self.config.export.format,
        })
    }

    fn mix_audio_pcm(&self, sample_rate_hz: u32, channels: u16) -> Result<Vec<i16>> {
        if self.config.export.format == ExportFormat::Gif {
            return Ok(Vec::new());
        }

        let system_input = self
            .manifest
            .audio_system_path
            .as_ref()
            .map(|path| Mp3SourceInput {
                path: path.clone(),
                enabled: self.config.system_audio.enabled && self.manifest.recorded_system_audio,
                volume: self.config.system_audio.volume,
            });
        let mic_input = self
            .manifest
            .audio_mic_path
            .as_ref()
            .map(|path| Mp3SourceInput {
                path: path.clone(),
                enabled: self.config.microphone_audio.enabled
                    && self.manifest.recorded_microphone_audio,
                volume: self.config.microphone_audio.volume,
            });

        mix_mp3_sources_to_pcm(
            system_input,
            mic_input,
            self.config.playback_speed,
            sample_rate_hz,
            channels,
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct QualityParams {
    scale: f32,
    export_fps: u32,
}

fn quality_params(record_fps: u32, quality: u8, format: ExportFormat) -> QualityParams {
    let q = quality.min(100) as f32 / 100.0;
    let scale = 0.5 + 0.5 * q;
    let fps_factor = 0.35 + 0.65 * q;
    let mut export_fps = ((record_fps.max(1) as f32) * fps_factor).round() as u32;
    export_fps = export_fps.clamp(10, record_fps.max(10));
    if matches!(format, ExportFormat::Gif) {
        export_fps = export_fps.min(20).max(1);
    }
    QualityParams { scale, export_fps }
}

fn scaled_dimensions(src_w: u32, src_h: u32, scale: f32, force_even: bool) -> (u32, u32) {
    let mut w = ((src_w as f32) * scale).round() as u32;
    let mut h = ((src_h as f32) * scale).round() as u32;
    w = w.clamp(1, src_w.max(1));
    h = h.clamp(1, src_h.max(1));

    if force_even {
        if w % 2 != 0 {
            w = w.saturating_sub(1).max(2);
        }
        if h % 2 != 0 {
            h = h.saturating_sub(1).max(2);
        }
    }

    (w, h)
}

fn retime_frames(
    source_frames: &[ReconstructedFrame],
    playback_speed: f32,
    export_fps: u32,
) -> Result<Vec<ReconstructedFrame>> {
    let speed = playback_speed.clamp(0.25, 4.0);
    let interval_ms = (1000.0 / export_fps.max(1) as f64).max(1.0);

    let mut starts = Vec::with_capacity(source_frames.len());
    let mut accumulated = 0u64;
    for frame in source_frames {
        starts.push(accumulated);
        accumulated = accumulated.saturating_add(u64::from(frame.duration_ms.max(1)));
    }
    if accumulated == 0 {
        return Ok(Vec::new());
    }
    let output_duration_ms = ((accumulated as f64) / speed as f64).ceil().max(1.0);

    let mut out = Vec::new();
    let mut src_idx = 0usize;
    let mut out_t = 0.0f64;
    while out_t < output_duration_ms {
        let src_t = (out_t * speed as f64).round() as u64;
        while src_idx + 1 < starts.len() && src_t >= starts[src_idx + 1] {
            src_idx += 1;
        }

        let mut frame = source_frames[src_idx].clone();
        frame.timestamp_ms = out_t.round() as u64;
        frame.duration_ms = interval_ms.round().max(1.0) as u32;
        out.push(frame);
        out_t += interval_ms;
    }

    if out.is_empty() {
        let mut frame = source_frames[0].clone();
        frame.duration_ms = output_duration_ms.round().max(1.0) as u32;
        out.push(frame);
    }

    Ok(out)
}

fn resize_rgba_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }

    let mut out = vec![0u8; dst_w as usize * dst_h as usize * 4];
    for y in 0..dst_h {
        let sy = ((y as u64 * src_h as u64) / dst_h as u64) as u32;
        for x in 0..dst_w {
            let sx = ((x as u64 * src_w as u64) / dst_w as u64) as u32;
            let src_idx = (sy as usize * src_w as usize + sx as usize) * 4;
            let dst_idx = (y as usize * dst_w as usize + x as usize) * 4;
            out[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
        }
    }
    out
}

#[derive(Clone, Debug)]
struct MouseSample {
    ts_ms: u64,
    x: i32,
    y: i32,
    visible: bool,
}

#[derive(Clone, Debug)]
struct MouseClickDown {
    ts_ms: u64,
    x: i32,
    y: i32,
}

#[derive(Clone, Debug, Default)]
struct MouseTracks {
    samples: Vec<MouseSample>,
    click_downs: Vec<MouseClickDown>,
}

fn build_mouse_tracks(pauses: &[PauseInterval], records: &[MouseRecord]) -> MouseTracks {
    let mut tracks = MouseTracks::default();

    for record in records {
        match record {
            MouseRecord::CursorSample(CursorSampleRecord {
                timestamp_ms,
                x,
                y,
                visible,
                ..
            }) => {
                tracks.samples.push(MouseSample {
                    ts_ms: map_wall_to_active(*timestamp_ms, pauses),
                    x: *x,
                    y: *y,
                    visible: *visible,
                });
            }
            MouseRecord::Click(ClickEventRecord {
                timestamp_ms,
                x,
                y,
                down,
                ..
            }) if *down => {
                tracks.click_downs.push(MouseClickDown {
                    ts_ms: map_wall_to_active(*timestamp_ms, pauses),
                    x: *x,
                    y: *y,
                });
            }
            _ => {}
        }
    }

    tracks.samples.sort_by_key(|s| s.ts_ms);
    tracks.click_downs.sort_by_key(|c| c.ts_ms);
    tracks
}

fn map_wall_to_active(timestamp_ms: u64, pauses: &[PauseInterval]) -> u64 {
    let mut active = timestamp_ms;
    for pause in pauses {
        if timestamp_ms >= pause.end_ms {
            active = active.saturating_sub(pause.end_ms.saturating_sub(pause.start_ms));
            continue;
        }
        if timestamp_ms > pause.start_ms {
            active = active.saturating_sub(timestamp_ms.saturating_sub(pause.start_ms));
            break;
        }
    }
    active
}

fn apply_mouse_overlays(
    frame: &mut ReconstructedFrame,
    tracks: &MouseTracks,
    config: &MouseEditConfig,
) {
    if tracks.samples.is_empty() {
        return;
    }

    let ts = frame.timestamp_ms;
    let cursor_idx = tracks.samples.partition_point(|s| s.ts_ms <= ts);
    if cursor_idx == 0 {
        return;
    }
    let current = &tracks.samples[cursor_idx - 1];

    if config.trail_enabled {
        draw_mouse_trail(frame, &tracks.samples[..cursor_idx], ts);
    }

    if config.click_enabled {
        draw_click_ripples(frame, &tracks.click_downs, ts);
    }

    if config.visible && current.visible {
        draw_cursor(frame, current.x, current.y);
    }
}

fn draw_mouse_trail(frame: &mut ReconstructedFrame, samples: &[MouseSample], ts: u64) {
    let trail_window_ms = 600u64;
    let cutoff = ts.saturating_sub(trail_window_ms);
    let recent: Vec<&MouseSample> = samples
        .iter()
        .rev()
        .take_while(|s| s.ts_ms >= cutoff)
        .collect();
    if recent.len() < 2 {
        return;
    }

    let mut ordered = recent;
    ordered.reverse();
    for win in ordered.windows(2) {
        let a = win[0];
        let b = win[1];
        let age = ts.saturating_sub(b.ts_ms).min(trail_window_ms);
        let alpha = ((1.0 - age as f32 / trail_window_ms as f32) * 180.0).round() as u8;
        draw_line(frame, a.x, a.y, b.x, b.y, [255, 32, 32, alpha], 2);
    }
}

fn draw_click_ripples(frame: &mut ReconstructedFrame, clicks: &[MouseClickDown], ts: u64) {
    let ripple_ms = 350u64;
    for click in clicks {
        if ts < click.ts_ms || ts > click.ts_ms + ripple_ms {
            continue;
        }
        let t = (ts - click.ts_ms) as f32 / ripple_ms as f32;
        let radius = (5.0 + 26.0 * t).round() as i32;
        let alpha = ((1.0 - t) * 220.0).round() as u8;
        draw_circle_outline(frame, click.x, click.y, radius, [255, 0, 0, alpha]);
    }
}

fn draw_cursor(frame: &mut ReconstructedFrame, x: i32, y: i32) {
    let points = [(x, y), (x + 12, y + 4), (x + 4, y + 12)];
    fill_triangle(frame, points, [255, 255, 255, 235]);
    draw_line(frame, x, y, x + 12, y + 4, [0, 0, 0, 200], 1);
    draw_line(frame, x, y, x + 4, y + 12, [0, 0, 0, 200], 1);
    draw_line(frame, x + 12, y + 4, x + 4, y + 12, [0, 0, 0, 200], 1);
}

fn set_pixel_blended(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let idx = (y as usize * width as usize + x as usize) * 4;
    let src_a = color[3] as f32 / 255.0;
    let inv = 1.0 - src_a;
    rgba[idx] = (color[0] as f32 * src_a + rgba[idx] as f32 * inv) as u8;
    rgba[idx + 1] = (color[1] as f32 * src_a + rgba[idx + 1] as f32 * inv) as u8;
    rgba[idx + 2] = (color[2] as f32 * src_a + rgba[idx + 2] as f32 * inv) as u8;
    rgba[idx + 3] = 255;
}

fn draw_line(
    frame: &mut ReconstructedFrame,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: [u8; 4],
    thickness: i32,
) {
    let mut x0 = x0;
    let mut y0 = y0;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        for oy in -thickness..=thickness {
            for ox in -thickness..=thickness {
                set_pixel_blended(
                    &mut frame.rgba,
                    frame.width,
                    frame.height,
                    x0 + ox,
                    y0 + oy,
                    color,
                );
            }
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = err * 2;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn draw_circle_outline(
    frame: &mut ReconstructedFrame,
    cx: i32,
    cy: i32,
    radius: i32,
    color: [u8; 4],
) {
    if radius <= 0 {
        return;
    }
    let mut x = radius;
    let mut y = 0;
    let mut err = 0;

    while x >= y {
        for (dx, dy) in [
            (x, y),
            (y, x),
            (-y, x),
            (-x, y),
            (-x, -y),
            (-y, -x),
            (y, -x),
            (x, -y),
        ] {
            set_pixel_blended(
                &mut frame.rgba,
                frame.width,
                frame.height,
                cx + dx,
                cy + dy,
                color,
            );
        }

        y += 1;
        if err <= 0 {
            err += 2 * y + 1;
        }
        if err > 0 {
            x -= 1;
            err -= 2 * x + 1;
        }
    }
}

fn fill_triangle(frame: &mut ReconstructedFrame, points: [(i32, i32); 3], color: [u8; 4]) {
    let min_x = points.iter().map(|p| p.0).min().unwrap_or(0);
    let max_x = points.iter().map(|p| p.0).max().unwrap_or(0);
    let min_y = points.iter().map(|p| p.1).min().unwrap_or(0);
    let max_y = points.iter().map(|p| p.1).max().unwrap_or(0);

    let area = edge(points[0], points[1], points[2]) as f32;
    if area.abs() < f32::EPSILON {
        return;
    }

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = (x, y);
            let w0 = edge(points[1], points[2], p) as f32 / area;
            let w1 = edge(points[2], points[0], p) as f32 / area;
            let w2 = edge(points[0], points[1], p) as f32 / area;
            if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                set_pixel_blended(&mut frame.rgba, frame.width, frame.height, x, y, color);
            }
        }
    }
}

fn edge(a: (i32, i32), b: (i32, i32), p: (i32, i32)) -> i32 {
    (p.0 - a.0) * (b.1 - a.1) - (p.1 - a.1) * (b.0 - a.0)
}

fn export_mp4(
    output_path: &Path,
    frames: &[ReconstructedFrame],
    pcm_audio: Vec<i16>,
    audio_bitrate_kbps: u16,
) -> Result<()> {
    if frames.is_empty() {
        return Err(ScreenRecorderError::Export(
            "MP4 export requires at least one frame".to_string(),
        ));
    }

    let mut file = File::create(output_path)?;
    let mut muxer = Mp4e::new(&mut file);
    muxer.set_video_track(frames[0].width, frames[0].height, Mp4Codec::AVC);
    if !pcm_audio.is_empty() {
        muxer.set_audio_track(48_000, 2, Mp4Codec::OPUS);
    }

    let mut encoder = H264LosslessAnnexBEncoder::new();
    for frame in frames {
        let annex_b = encoder.encode_rgba(frame.width, frame.height, &frame.rgba)?;
        muxer
            .encode_video(&annex_b, frame.duration_ms.max(1))
            .map_err(|e| ScreenRecorderError::Export(format!("failed to mux MP4 video: {e}")))?;
    }

    if !pcm_audio.is_empty() {
        let mut opus = OpusEncoderWrapper::new(48_000, 2, audio_bitrate_kbps)?;
        let frame_samples = opus.frame_samples_per_channel();
        let packet_samples = frame_samples * 2;
        for chunk in pcm_audio.chunks(packet_samples) {
            let mut packet_pcm = vec![0i16; packet_samples];
            packet_pcm[..chunk.len()].copy_from_slice(chunk);
            let encoded = opus.encode_frame(&packet_pcm)?;
            muxer
                .encode_audio(&encoded, frame_samples as u32)
                .map_err(|e| {
                    ScreenRecorderError::Export(format!("failed to mux MP4 audio: {e}"))
                })?;
        }
    }

    muxer
        .flush()
        .map_err(|e| ScreenRecorderError::Export(format!("failed to finalize MP4: {e}")))?;
    file.flush()?;
    Ok(())
}

fn export_avi(
    output_path: &Path,
    frames: &[ReconstructedFrame],
    export_fps: u32,
    pcm_audio: Vec<i16>,
    audio_sample_rate_hz: u32,
    audio_channels: u16,
    audio_bitrate_kbps: u16,
    quality: u8,
) -> Result<()> {
    if frames.is_empty() {
        return Err(ScreenRecorderError::Export(
            "AVI export requires at least one frame".to_string(),
        ));
    }

    let jpeg_quality = quality.max(5);
    let mut mjpeg_frames = Vec::with_capacity(frames.len());
    for frame in frames {
        let mut out = Vec::new();
        let encoder = JpegEncoder::new(&mut out, jpeg_quality);
        encoder
            .encode(
                &frame.rgba,
                frame.width as u16,
                frame.height as u16,
                JpegColorType::Rgba,
            )
            .map_err(|e| ScreenRecorderError::Encode(format!("JPEG encode failed: {e}")))?;
        mjpeg_frames.push(out);
    }

    let audio_track = if pcm_audio.is_empty() {
        None
    } else {
        let mp3_data = encode_pcm_to_mp3(
            &pcm_audio,
            audio_sample_rate_hz,
            audio_channels,
            audio_bitrate_kbps,
        )?;
        Some(AviAudioTrack {
            mp3_data,
            sample_rate_hz: audio_sample_rate_hz,
            channels: audio_channels,
            bitrate_kbps: audio_bitrate_kbps,
        })
    };

    write_avi(
        output_path,
        frames[0].width,
        frames[0].height,
        export_fps.max(1),
        &mjpeg_frames,
        audio_track.as_ref(),
    )
}

fn export_gif(output_path: &Path, frames: &[ReconstructedFrame], quality: u8) -> Result<()> {
    if frames.is_empty() {
        return Err(ScreenRecorderError::Export(
            "GIF export requires at least one frame".to_string(),
        ));
    }

    if frames[0].width > u16::MAX as u32 || frames[0].height > u16::MAX as u32 {
        return Err(ScreenRecorderError::Export(
            "GIF export dimensions exceed u16 limits".to_string(),
        ));
    }

    let mut file = File::create(output_path)?;
    let mut encoder = GifEncoder::new(
        &mut file,
        frames[0].width as u16,
        frames[0].height as u16,
        &[],
    )
    .map_err(|e| ScreenRecorderError::Export(format!("failed to create GIF encoder: {e}")))?;
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(|e| ScreenRecorderError::Export(format!("failed to set GIF repeat: {e}")))?;

    let palette_size = 16 + ((quality as u16 * 240) / 100);
    let quant_speed = (31 - ((palette_size as i32 * 30) / 256)).clamp(1, 30);

    for frame in frames {
        let mut rgba = frame.rgba.clone();
        let mut gif_frame = GifFrame::from_rgba_speed(
            frame.width as u16,
            frame.height as u16,
            &mut rgba,
            quant_speed,
        );
        gif_frame.delay = ((frame.duration_ms as f32) / 10.0).round().max(1.0) as u16;
        encoder
            .write_frame(&gif_frame)
            .map_err(|e| ScreenRecorderError::Export(format!("failed to write GIF frame: {e}")))?;
    }

    drop(encoder);
    file.flush()?;
    Ok(())
}

fn validate_manifest_paths(manifest: &SessionManifest) -> Result<()> {
    validate_file_exists(&manifest.frame_cache_path)?;
    validate_file_exists(&manifest.mouse_path)?;
    validate_file_exists(&manifest.video_temp_path)?;

    if manifest.recorded_system_audio {
        if let Some(path) = manifest.audio_system_path.as_ref() {
            validate_file_exists(path)?;
        } else {
            return Err(ScreenRecorderError::Decode(
                "manifest indicates system audio was recorded but path is missing".to_string(),
            ));
        }
    }

    if manifest.recorded_microphone_audio {
        if let Some(path) = manifest.audio_mic_path.as_ref() {
            validate_file_exists(path)?;
        } else {
            return Err(ScreenRecorderError::Decode(
                "manifest indicates microphone audio was recorded but path is missing".to_string(),
            ));
        }
    }

    Ok(())
}

fn validate_file_exists(path: &Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    Err(ScreenRecorderError::Decode(format!(
        "required artifact is missing: {}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_endpoints_match_plan() {
        let low = quality_params(60, 0, ExportFormat::Mp4);
        let high = quality_params(60, 100, ExportFormat::Mp4);
        assert!((low.scale - 0.5).abs() < 1e-6);
        assert_eq!(low.export_fps, 21);
        assert!((high.scale - 1.0).abs() < 1e-6);
        assert_eq!(high.export_fps, 60);
    }

    #[test]
    fn map_wall_time_excludes_pause_gaps() {
        let pauses = vec![
            PauseInterval {
                start_ms: 100,
                end_ms: 200,
            },
            PauseInterval {
                start_ms: 400,
                end_ms: 500,
            },
        ];
        assert_eq!(map_wall_to_active(50, &pauses), 50);
        assert_eq!(map_wall_to_active(250, &pauses), 150);
        assert_eq!(map_wall_to_active(450, &pauses), 300);
        assert_eq!(map_wall_to_active(800, &pauses), 600);
    }
}
