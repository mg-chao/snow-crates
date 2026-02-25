use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use ffmpeg_next as ffmpeg;
use snow_audio_recorder::align_i16_interleaved_to_duration;

use crate::artifact::{RecordingArtifact, SessionManifest};
use crate::config::{EditConfig, ExportFormat, MouseEditConfig, VideoEncodeConfig};
use crate::error::{Result, ScreenRecorderError};
use crate::export::ExportResult;
use crate::ffmpeg_util::{copy_rgba_into_frame, ensure_ffmpeg_initialized, is_eagain};
use crate::model::StoredFrame;
use crate::mouse::{CursorShapeCompositionMode, CursorShapeRecord, MouseStore, read_mouse_records};
use crate::video_quality::{quality_to_h264_crf, smart_quality_bitrate_bps};

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
        config.export.video = manifest.recording_video.clone();
        config.export.output_path = manifest
            .output_dir
            .join(format!("{}.mp4", manifest.session_id));

        Ok(Self {
            artifact,
            manifest,
            config,
        })
    }

    pub fn set_config(&mut self, mut config: EditConfig) -> Result<()> {
        config
            .validate()
            .map_err(ScreenRecorderError::InvalidConfig)?;

        if config.system_audio.enabled && !self.manifest.recorded_system_audio {
            config.system_audio.enabled = false;
        }
        if config.microphone_audio.enabled && !self.manifest.recorded_microphone_audio {
            config.microphone_audio.enabled = false;
        }

        self.config = config;
        Ok(())
    }

    pub fn export(self) -> Result<ExportResult> {
        self.config
            .validate()
            .map_err(ScreenRecorderError::InvalidConfig)?;

        let temp_dir = self.artifact.temp_dir.clone();
        let keep_temp_files = self.manifest.keep_temp_files;
        let result = self.export_inner();
        if result.is_ok() && !keep_temp_files {
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
        if let Some(parent) = self.config.export.output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let source_frames = decode_video_frames(&self.manifest.video_temp_path, self.manifest.fps)?;
        if source_frames.is_empty() {
            return Err(ScreenRecorderError::Export(
                "no frames available for export".to_string(),
            ));
        }

        let mouse_store = read_mouse_records(&self.manifest.mouse_path)?;
        let mouse_tracks = build_mouse_tracks(&mouse_store);

        let export_fps = choose_export_fps(self.manifest.fps, self.config.export.format);
        let mut frames = retime_frames(&source_frames, self.config.playback_speed, export_fps)?;
        if frames.is_empty() {
            return Err(ScreenRecorderError::Export(
                "retiming produced no frames".to_string(),
            ));
        }

        let (output_w, output_h) = output_dimensions(
            source_frames[0].width,
            source_frames[0].height,
            self.config.export.format != ExportFormat::Gif,
        );
        for frame in &mut frames {
            apply_mouse_overlays(frame, &mouse_tracks, &self.config.mouse);
            if frame.width != output_w || frame.height != output_h {
                frame.rgba =
                    resize_rgba_nearest(&frame.rgba, frame.width, frame.height, output_w, output_h);
                frame.width = output_w;
                frame.height = output_h;
            }
        }

        let duration_ms = frames
            .iter()
            .map(|f| u64::from(f.duration_ms.max(1)))
            .sum::<u64>()
            .max(1);

        let mixed_audio = if self.config.export.format == ExportFormat::Gif {
            None
        } else {
            build_mixed_audio(&self.manifest, &self.config, duration_ms)?
        };

        match self.config.export.format {
            ExportFormat::Mp4 => export_video(
                &self.config.export.output_path,
                &frames,
                export_fps,
                ExportFormat::Mp4,
                mixed_audio.as_ref(),
                self.manifest.audio_bitrate_kbps.max(8),
                &self.config.export.video,
            )?,
            ExportFormat::Avi => export_video(
                &self.config.export.output_path,
                &frames,
                export_fps,
                ExportFormat::Avi,
                mixed_audio.as_ref(),
                self.manifest.audio_bitrate_kbps.max(8),
                &self.config.export.video,
            )?,
            ExportFormat::Gif => export_video(
                &self.config.export.output_path,
                &frames,
                export_fps,
                ExportFormat::Gif,
                None,
                self.manifest.audio_bitrate_kbps.max(8),
                &self.config.export.video,
            )?,
        }

        Ok(ExportResult {
            output_path: self.config.export.output_path,
            duration_ms,
            format: self.config.export.format,
        })
    }
}

fn choose_export_fps(record_fps: u32, format: ExportFormat) -> u32 {
    let fps = record_fps.max(1);
    if matches!(format, ExportFormat::Gif) {
        fps.min(20).max(1)
    } else {
        fps
    }
}

fn output_dimensions(src_w: u32, src_h: u32, force_even: bool) -> (u32, u32) {
    let mut w = src_w.max(1);
    let mut h = src_h.max(1);

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

fn decode_video_frames(path: &Path, fallback_fps: u32) -> Result<Vec<StoredFrame>> {
    ensure_ffmpeg_initialized()?;

    let mut input = ffmpeg::format::input(path).map_err(|err| {
        ScreenRecorderError::Export(format!(
            "failed to open temporary recording video {}: {err}",
            path.display()
        ))
    })?;

    let video_stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| {
            ScreenRecorderError::Export(format!(
                "temporary recording video {} has no video stream",
                path.display()
            ))
        })?;
    let stream_index = video_stream.index();
    let stream_time_base = video_stream.time_base();

    let context = ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())
        .map_err(|err| {
            ScreenRecorderError::Export(format!(
                "failed to create decoder context for {}: {err}",
                path.display()
            ))
        })?;
    let mut decoder = context.decoder().video().map_err(|err| {
        ScreenRecorderError::Export(format!(
            "failed to open temporary recording video decoder for {}: {err}",
            path.display()
        ))
    })?;

    let nominal_duration_ms = ((1000.0 / fallback_fps.max(1) as f64).round() as u32).max(1);
    let mut scaler = None::<ffmpeg::software::scaling::Context>;
    let mut rgba_frame = None::<ffmpeg::frame::Video>;
    let mut last_timestamp_ms = None::<u64>;
    let mut frames = Vec::<StoredFrame>::new();

    let mut decoded = ffmpeg::frame::Video::empty();
    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }

        decoder.send_packet(&packet).map_err(|err| {
            ScreenRecorderError::Export(format!(
                "failed to feed packet into temporary recording video decoder: {err}"
            ))
        })?;

        loop {
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    push_decoded_video_frame(
                        &decoded,
                        stream_time_base,
                        nominal_duration_ms,
                        &mut scaler,
                        &mut rgba_frame,
                        &mut last_timestamp_ms,
                        &mut frames,
                    )?;
                }
                Err(err) if is_eagain(&err) => break,
                Err(err) if err == ffmpeg::Error::Eof => break,
                Err(err) => {
                    return Err(ScreenRecorderError::Export(format!(
                        "failed to decode temporary recording video frame: {err}"
                    )));
                }
            }
        }
    }

    decoder.send_eof().map_err(|err| {
        ScreenRecorderError::Export(format!(
            "failed to flush temporary recording video decoder: {err}"
        ))
    })?;
    loop {
        match decoder.receive_frame(&mut decoded) {
            Ok(()) => {
                push_decoded_video_frame(
                    &decoded,
                    stream_time_base,
                    nominal_duration_ms,
                    &mut scaler,
                    &mut rgba_frame,
                    &mut last_timestamp_ms,
                    &mut frames,
                )?;
            }
            Err(err) if is_eagain(&err) => continue,
            Err(err) if err == ffmpeg::Error::Eof => break,
            Err(err) => {
                return Err(ScreenRecorderError::Export(format!(
                    "failed to drain temporary recording video decoder: {err}"
                )));
            }
        }
    }

    if frames.is_empty() {
        return Err(ScreenRecorderError::Export(format!(
            "temporary recording video {} contains no decodable frames",
            path.display()
        )));
    }

    stamp_frame_durations(&mut frames, nominal_duration_ms);
    Ok(frames)
}

fn push_decoded_video_frame(
    decoded: &ffmpeg::frame::Video,
    stream_time_base: ffmpeg::Rational,
    nominal_duration_ms: u32,
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    rgba_frame: &mut Option<ffmpeg::frame::Video>,
    last_timestamp_ms: &mut Option<u64>,
    frames: &mut Vec<StoredFrame>,
) -> Result<()> {
    let width = decoded.width();
    let height = decoded.height();
    if width == 0 || height == 0 {
        return Ok(());
    }

    let needs_reset = scaler.is_none()
        || rgba_frame
            .as_ref()
            .map(|frame| frame.width() != width || frame.height() != height)
            .unwrap_or(false);
    if needs_reset {
        *scaler = Some(
            ffmpeg::software::scaling::Context::get(
                decoded.format(),
                width,
                height,
                ffmpeg::format::Pixel::RGBA,
                width,
                height,
                ffmpeg::software::scaling::flag::Flags::BILINEAR,
            )
            .map_err(|err| {
                ScreenRecorderError::Export(format!(
                    "failed to create temporary video decode scaler: {err}"
                ))
            })?,
        );
        *rgba_frame = Some(ffmpeg::frame::Video::new(
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
        ));
    }

    let scaler_ref = scaler
        .as_mut()
        .ok_or_else(|| ScreenRecorderError::Export("video scaler is uninitialized".to_string()))?;
    let rgba_ref = rgba_frame.as_mut().ok_or_else(|| {
        ScreenRecorderError::Export("video frame buffer is uninitialized".to_string())
    })?;
    scaler_ref.run(decoded, rgba_ref).map_err(|err| {
        ScreenRecorderError::Export(format!(
            "failed to convert decoded temporary video frame into RGBA: {err}"
        ))
    })?;

    let mut timestamp_ms = decoded
        .timestamp()
        .or_else(|| decoded.pts())
        .map(|pts| pts_to_millis(pts, stream_time_base))
        .unwrap_or_else(|| {
            last_timestamp_ms
                .unwrap_or(0)
                .saturating_add(u64::from(nominal_duration_ms))
        });
    if let Some(previous) = *last_timestamp_ms
        && timestamp_ms <= previous
    {
        timestamp_ms = previous.saturating_add(1);
    }
    *last_timestamp_ms = Some(timestamp_ms);

    frames.push(StoredFrame {
        timestamp_ms,
        duration_ms: nominal_duration_ms.max(1),
        width,
        height,
        rgba: extract_rgba_from_frame(rgba_ref, width, height)?,
    });

    Ok(())
}

fn extract_rgba_from_frame(
    frame: &ffmpeg::frame::Video,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let stride = frame.stride(0);
    let row_bytes = width as usize * 4;
    let height_usize = height as usize;
    let src = frame.data(0);

    let mut rgba = vec![0u8; row_bytes * height_usize];
    for y in 0..height_usize {
        let src_start = y * stride;
        let src_end = src_start + row_bytes;
        if src_end > src.len() {
            return Err(ScreenRecorderError::Export(
                "decoded RGBA frame stride exceeds available data".to_string(),
            ));
        }
        let dst_start = y * row_bytes;
        let dst_end = dst_start + row_bytes;
        rgba[dst_start..dst_end].copy_from_slice(&src[src_start..src_end]);
    }

    Ok(rgba)
}

fn pts_to_millis(pts: i64, time_base: ffmpeg::Rational) -> u64 {
    let numerator = i128::from(time_base.numerator().max(1));
    let denominator = i128::from(time_base.denominator().max(1));
    let millis = i128::from(pts)
        .saturating_mul(numerator)
        .saturating_mul(1_000)
        / denominator;
    millis.clamp(0, i128::from(u64::MAX)) as u64
}

fn stamp_frame_durations(frames: &mut [StoredFrame], nominal_duration_ms: u32) {
    if frames.is_empty() {
        return;
    }

    for idx in 0..frames.len().saturating_sub(1) {
        let delta = frames[idx + 1]
            .timestamp_ms
            .saturating_sub(frames[idx].timestamp_ms);
        frames[idx].duration_ms = delta.max(1).min(u64::from(u32::MAX)) as u32;
    }

    if let Some(last) = frames.last_mut() {
        last.duration_ms = nominal_duration_ms.max(1);
    }
}

fn retime_frames(
    source_frames: &[StoredFrame],
    playback_speed: f32,
    export_fps: u32,
) -> Result<Vec<StoredFrame>> {
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
    shape_id: Option<u64>,
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
    cursor_shapes: HashMap<u64, CursorShapeRecord>,
}

fn build_mouse_tracks(store: &MouseStore) -> MouseTracks {
    let mut tracks = MouseTracks::default();

    for sample in &store.cursor_frames {
        tracks.samples.push(MouseSample {
            ts_ms: sample.timestamp_ms,
            x: sample.x,
            y: sample.y,
            visible: sample.visible,
            shape_id: sample.shape_id,
        });
    }
    for shape in &store.cursor_shapes {
        tracks
            .cursor_shapes
            .entry(shape.shape_id)
            .or_insert_with(|| shape.clone());
    }
    for click in &store.clicks {
        if click.down {
            tracks.click_downs.push(MouseClickDown {
                ts_ms: click.timestamp_ms,
                x: click.x,
                y: click.y,
            });
        }
    }

    tracks.samples.sort_by_key(|s| s.ts_ms);
    tracks.click_downs.sort_by_key(|c| c.ts_ms);
    tracks
}

fn apply_mouse_overlays(frame: &mut StoredFrame, tracks: &MouseTracks, config: &MouseEditConfig) {
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
        draw_mouse_trail(frame, &tracks.samples[..cursor_idx], ts, config);
    }
    if config.click_enabled {
        draw_click_ripples(frame, &tracks.click_downs, ts);
    }
    if config.visible && current.visible {
        draw_cursor(frame, current, tracks);
    }
}

fn draw_mouse_trail(
    frame: &mut StoredFrame,
    samples: &[MouseSample],
    ts: u64,
    config: &MouseEditConfig,
) {
    let trail_window_ms = config.trail_window_ms.max(1);
    let cutoff = ts.saturating_sub(trail_window_ms);
    let visible = collect_visible_trail_window(samples, cutoff);
    if visible.len() < 2 {
        return;
    }

    let smoothed = build_smoothed_trail_points(&visible, config.trail_smooth_step_px);
    if smoothed.len() < 2 {
        return;
    }

    for segment in smoothed.windows(2) {
        let a = segment[0];
        let b = segment[1];
        let b_ts_ms = b.ts_ms.max(0.0).round() as u64;
        let age = ts.saturating_sub(b_ts_ms).min(trail_window_ms);
        let alpha = ((1.0 - age as f32 / trail_window_ms as f32) * config.trail_max_alpha as f32)
            .round()
            .clamp(0.0, 255.0) as u8;
        if alpha == 0 {
            continue;
        }
        draw_line(
            frame,
            a.x.round() as i32,
            a.y.round() as i32,
            b.x.round() as i32,
            b.y.round() as i32,
            [
                config.trail_color[0],
                config.trail_color[1],
                config.trail_color[2],
                alpha,
            ],
            config.trail_thickness.max(0),
        );
    }
}

#[derive(Clone, Copy)]
struct TrailCurvePoint {
    x: f32,
    y: f32,
    ts_ms: f32,
}

fn collect_visible_trail_window(samples: &[MouseSample], cutoff_ms: u64) -> Vec<TrailCurvePoint> {
    let visible: Vec<TrailCurvePoint> = samples
        .iter()
        .filter(|s| s.visible)
        .map(|s| TrailCurvePoint {
            x: s.x as f32,
            y: s.y as f32,
            ts_ms: s.ts_ms as f32,
        })
        .collect();
    if visible.is_empty() {
        return visible;
    }

    let cutoff_ms = cutoff_ms as f32;
    let first_in_window = visible.partition_point(|p| p.ts_ms < cutoff_ms);
    if first_in_window == 0 {
        return visible;
    }
    if first_in_window >= visible.len() {
        return Vec::new();
    }

    let prev = visible[first_in_window - 1];
    let next = visible[first_in_window];
    let mut window = Vec::with_capacity(visible.len() - first_in_window + 1);
    if cutoff_ms > prev.ts_ms && cutoff_ms < next.ts_ms {
        let t = (cutoff_ms - prev.ts_ms) / (next.ts_ms - prev.ts_ms);
        window.push(TrailCurvePoint {
            x: prev.x + (next.x - prev.x) * t,
            y: prev.y + (next.y - prev.y) * t,
            ts_ms: cutoff_ms,
        });
    }
    window.extend_from_slice(&visible[first_in_window..]);
    window
}

fn build_smoothed_trail_points(samples: &[TrailCurvePoint], step_px: f32) -> Vec<TrailCurvePoint> {
    if samples.is_empty() {
        return Vec::new();
    }

    let step_px = step_px.max(1.0);
    let mut points = Vec::new();
    let first = samples[0];
    points.push(first);
    if samples.len() == 1 {
        return points;
    }

    if samples.len() == 2 {
        append_linear_segment(&mut points, first, samples[1], step_px);
        return points;
    }

    let first_mid = midpoint(samples[0], samples[1]);
    append_linear_segment(&mut points, first, first_mid, step_px);

    for idx in 1..samples.len() - 1 {
        let prev = samples[idx - 1];
        let current = samples[idx];
        let next = samples[idx + 1];
        let start = midpoint(prev, current);
        let end = midpoint(current, next);
        append_quadratic_segment(&mut points, start, current, end, step_px);
    }

    let last_mid = midpoint(samples[samples.len() - 2], samples[samples.len() - 1]);
    let last = samples[samples.len() - 1];
    append_linear_segment(&mut points, last_mid, last, step_px);
    points
}

fn midpoint(a: TrailCurvePoint, b: TrailCurvePoint) -> TrailCurvePoint {
    TrailCurvePoint {
        x: (a.x + b.x) * 0.5,
        y: (a.y + b.y) * 0.5,
        ts_ms: (a.ts_ms + b.ts_ms) * 0.5,
    }
}

fn append_linear_segment(
    out: &mut Vec<TrailCurvePoint>,
    start: TrailCurvePoint,
    end: TrailCurvePoint,
    step_px: f32,
) {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let dist = (dx * dx + dy * dy).sqrt();
    let steps = (dist / step_px).ceil().max(1.0) as usize;

    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let x = start.x + (end.x - start.x) * t;
        let y = start.y + (end.y - start.y) * t;
        let ts_ms = start.ts_ms + (end.ts_ms - start.ts_ms) * t;
        out.push(TrailCurvePoint { x, y, ts_ms });
    }
}

fn append_quadratic_segment(
    out: &mut Vec<TrailCurvePoint>,
    start: TrailCurvePoint,
    control: TrailCurvePoint,
    end: TrailCurvePoint,
    step_px: f32,
) {
    let approx_len = ((control.x - start.x).powi(2) + (control.y - start.y).powi(2)).sqrt()
        + ((end.x - control.x).powi(2) + (end.y - control.y).powi(2)).sqrt();
    let steps = (approx_len / step_px).ceil().max(1.0) as usize;

    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let inv = 1.0 - t;
        let x = inv * inv * start.x + 2.0 * inv * t * control.x + t * t * end.x;
        let y = inv * inv * start.y + 2.0 * inv * t * control.y + t * t * end.y;
        let ts_ms = inv * inv * start.ts_ms + 2.0 * inv * t * control.ts_ms + t * t * end.ts_ms;
        out.push(TrailCurvePoint { x, y, ts_ms });
    }
}

fn draw_click_ripples(frame: &mut StoredFrame, clicks: &[MouseClickDown], ts: u64) {
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

fn draw_cursor(frame: &mut StoredFrame, current: &MouseSample, tracks: &MouseTracks) {
    if let Some(shape_id) = current.shape_id
        && let Some(shape) = tracks.cursor_shapes.get(&shape_id)
    {
        let width = shape.width as usize;
        let height = shape.height as usize;
        let expected_len = width
            .checked_mul(height)
            .and_then(|px| px.checked_mul(4))
            .unwrap_or(0);
        if expected_len > 0 && shape.shape_rgba.len() >= expected_len {
            let shape_rgba = &shape.shape_rgba[..expected_len];
            if !cursor_shape_is_renderable(shape.mode, shape_rgba) {
                draw_fallback_cursor(frame, current.x, current.y);
                return;
            }
            draw_cursor_shape(
                frame,
                current.x.saturating_sub(shape.hotspot_x as i32),
                current.y.saturating_sub(shape.hotspot_y as i32),
                width,
                height,
                shape.mode,
                shape_rgba,
            );
            return;
        }
    }

    draw_fallback_cursor(frame, current.x, current.y);
}

fn cursor_shape_is_renderable(mode: CursorShapeCompositionMode, rgba: &[u8]) -> bool {
    match mode {
        // Some Windows color cursors report a shape with fully transparent alpha.
        // In that case rendering the sampled bitmap is a no-op, so we should
        // fall back to the synthetic cursor.
        CursorShapeCompositionMode::AlphaBlend => rgba.chunks_exact(4).any(|px| px[3] != 0),
        CursorShapeCompositionMode::MaskedColor => masked_color_shape_has_visible_effect(rgba),
    }
}

fn masked_color_shape_has_visible_effect(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).any(|px| {
        let alpha = px[3];
        alpha != 0xFF || px[0] != 0 || px[1] != 0 || px[2] != 0
    })
}

fn draw_cursor_shape(
    frame: &mut StoredFrame,
    origin_x: i32,
    origin_y: i32,
    width: usize,
    height: usize,
    mode: CursorShapeCompositionMode,
    rgba: &[u8],
) {
    match mode {
        CursorShapeCompositionMode::AlphaBlend => {
            draw_alpha_blended_cursor_shape(frame, origin_x, origin_y, width, height, rgba)
        }
        CursorShapeCompositionMode::MaskedColor => {
            draw_masked_color_cursor_shape(frame, origin_x, origin_y, width, height, rgba)
        }
    }
}

fn draw_alpha_blended_cursor_shape(
    frame: &mut StoredFrame,
    origin_x: i32,
    origin_y: i32,
    width: usize,
    height: usize,
    rgba: &[u8],
) {
    for y in 0..height {
        let dst_y = origin_y.saturating_add(y as i32);
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let alpha = rgba[idx + 3];
            if alpha == 0 {
                continue;
            }
            set_pixel_blended(
                &mut frame.rgba,
                frame.width,
                frame.height,
                origin_x.saturating_add(x as i32),
                dst_y,
                [rgba[idx], rgba[idx + 1], rgba[idx + 2], alpha],
            );
        }
    }
}

fn draw_masked_color_cursor_shape(
    frame: &mut StoredFrame,
    origin_x: i32,
    origin_y: i32,
    width: usize,
    height: usize,
    rgba: &[u8],
) {
    for y in 0..height {
        let dst_y = origin_y.saturating_add(y as i32);
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let color = [rgba[idx], rgba[idx + 1], rgba[idx + 2]];
            match rgba[idx + 3] {
                0x00 => set_pixel_copy(
                    &mut frame.rgba,
                    frame.width,
                    frame.height,
                    origin_x + x as i32,
                    dst_y,
                    color,
                ),
                0xFF => {
                    set_pixel_xor(
                        &mut frame.rgba,
                        frame.width,
                        frame.height,
                        origin_x + x as i32,
                        dst_y,
                        color,
                    );
                }
                alpha => set_pixel_blended(
                    &mut frame.rgba,
                    frame.width,
                    frame.height,
                    origin_x + x as i32,
                    dst_y,
                    [color[0], color[1], color[2], alpha],
                ),
            }
        }
    }
}

fn draw_fallback_cursor(frame: &mut StoredFrame, x: i32, y: i32) {
    let points = [(x, y), (x + 12, y + 4), (x + 4, y + 12)];
    fill_triangle(frame, points, [255, 255, 255, 235]);
    draw_line(frame, x, y, x + 12, y + 4, [0, 0, 0, 200], 1);
    draw_line(frame, x, y, x + 4, y + 12, [0, 0, 0, 200], 1);
    draw_line(frame, x + 12, y + 4, x + 4, y + 12, [0, 0, 0, 200], 1);
}

fn pixel_offset(width: u32, height: u32, x: i32, y: i32) -> Option<usize> {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return None;
    }
    Some((y as usize * width as usize + x as usize) * 4)
}

fn set_pixel_copy(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 3]) {
    let Some(idx) = pixel_offset(width, height, x, y) else {
        return;
    };
    rgba[idx] = color[0];
    rgba[idx + 1] = color[1];
    rgba[idx + 2] = color[2];
    rgba[idx + 3] = 255;
}

fn set_pixel_xor(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, mask: [u8; 3]) {
    let Some(idx) = pixel_offset(width, height, x, y) else {
        return;
    };
    rgba[idx] ^= mask[0];
    rgba[idx + 1] ^= mask[1];
    rgba[idx + 2] ^= mask[2];
    rgba[idx + 3] = 255;
}

fn set_pixel_blended(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 4]) {
    let Some(idx) = pixel_offset(width, height, x, y) else {
        return;
    };
    let src_a = color[3] as f32 / 255.0;
    let inv = 1.0 - src_a;
    rgba[idx] = (color[0] as f32 * src_a + rgba[idx] as f32 * inv) as u8;
    rgba[idx + 1] = (color[1] as f32 * src_a + rgba[idx + 1] as f32 * inv) as u8;
    rgba[idx + 2] = (color[2] as f32 * src_a + rgba[idx + 2] as f32 * inv) as u8;
    rgba[idx + 3] = 255;
}

fn draw_line(
    frame: &mut StoredFrame,
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

fn draw_circle_outline(frame: &mut StoredFrame, cx: i32, cy: i32, radius: i32, color: [u8; 4]) {
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

fn fill_triangle(frame: &mut StoredFrame, points: [(i32, i32); 3], color: [u8; 4]) {
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

#[derive(Clone, Debug)]
struct MixedAudio {
    sample_rate_hz: u32,
    channels: u16,
    samples_i16: Vec<i16>,
}

fn build_mixed_audio(
    manifest: &SessionManifest,
    config: &EditConfig,
    target_duration_ms: u64,
) -> Result<Option<MixedAudio>> {
    let mut tracks = Vec::<(Vec<i16>, f32)>::new();
    let channels = manifest.audio_channels.max(1);

    if config.system_audio.enabled
        && let Some(path) = manifest.audio_system_path.as_ref()
    {
        let samples = read_pcm_i16(path, channels)?;
        if !samples.is_empty() {
            tracks.push((samples, config.system_audio.volume.clamp(0.0, 2.0)));
        }
    }

    if config.microphone_audio.enabled
        && let Some(path) = manifest.audio_mic_path.as_ref()
    {
        let samples = read_pcm_i16(path, channels)?;
        if !samples.is_empty() {
            tracks.push((samples, config.microphone_audio.volume.clamp(0.0, 2.0)));
        }
    }

    if tracks.is_empty() {
        return Ok(None);
    }

    let mixed = mix_audio_tracks_i16_interleaved(&tracks, channels);
    if mixed.is_empty() {
        return Ok(None);
    }

    let retimed = retime_audio_i16_interleaved(&mixed, channels, config.playback_speed);
    let aligned = align_i16_interleaved_to_duration(
        retimed,
        manifest.audio_sample_rate_hz.max(1),
        channels,
        Duration::from_millis(target_duration_ms),
    );
    if aligned.is_empty() {
        return Ok(None);
    }

    Ok(Some(MixedAudio {
        sample_rate_hz: manifest.audio_sample_rate_hz.max(1),
        channels,
        samples_i16: aligned,
    }))
}

fn read_pcm_i16(path: &Path, channels: u16) -> Result<Vec<i16>> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.len() % 2 != 0 {
        return Err(ScreenRecorderError::Decode(format!(
            "pcm file {} has odd byte length",
            path.display()
        )));
    }

    let mut samples = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    let channels = usize::from(channels.max(1));
    let aligned = (samples.len() / channels) * channels;
    samples.truncate(aligned);
    Ok(samples)
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



fn choose_video_codec_id(format: ExportFormat) -> ffmpeg::codec::Id {
    match format {
        ExportFormat::Mp4 => ffmpeg::codec::Id::H264,
        ExportFormat::Avi => ffmpeg::codec::Id::MPEG4,
        ExportFormat::Gif => ffmpeg::codec::Id::GIF,
    }
}

fn choose_video_pixel_format(
    format: ExportFormat,
    codec: ffmpeg::codec::Video,
) -> ffmpeg::format::Pixel {
    let preferred = match format {
        ExportFormat::Gif => [
            ffmpeg::format::Pixel::RGB8,
            ffmpeg::format::Pixel::PAL8,
            ffmpeg::format::Pixel::RGB24,
        ]
        .as_slice(),
        ExportFormat::Mp4 | ExportFormat::Avi => [
            ffmpeg::format::Pixel::YUV420P,
            ffmpeg::format::Pixel::YUV422P,
            ffmpeg::format::Pixel::RGB24,
        ]
        .as_slice(),
    };

    if let Some(formats) = codec.formats() {
        let available: Vec<_> = formats.collect();
        for pixel in preferred {
            if available.iter().any(|fmt| *fmt == *pixel) {
                return *pixel;
            }
        }
        if let Some(first) = available.first().copied() {
            return first;
        }
    }

    match format {
        ExportFormat::Gif => ffmpeg::format::Pixel::RGB8,
        ExportFormat::Mp4 | ExportFormat::Avi => ffmpeg::format::Pixel::YUV420P,
    }
}

fn choose_audio_codec(format: ExportFormat) -> Option<ffmpeg::Codec> {
    match format {
        ExportFormat::Mp4 => ffmpeg::encoder::find(ffmpeg::codec::Id::AAC),
        ExportFormat::Avi => ffmpeg::encoder::find_by_name("libmp3lame")
            .or_else(|| ffmpeg::encoder::find_by_name("libshine"))
            .or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::MP3))
            .or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::AAC)),
        ExportFormat::Gif => None,
    }
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

fn export_video(
    output_path: &Path,
    frames: &[StoredFrame],
    export_fps: u32,
    format: ExportFormat,
    mixed_audio: Option<&MixedAudio>,
    audio_bitrate_kbps: u16,
    video_config: &VideoEncodeConfig,
) -> Result<()> {
    ensure_ffmpeg_initialized()?;
    let (width, height) = validate_export_frames(frames, format != ExportFormat::Gif)?;

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

    let container_video_codec = output
        .format()
        .codec(output_path, ffmpeg::media::Type::Video);
    let preferred_video_codec = choose_video_codec_id(format);
    let video_codec = ffmpeg::encoder::find(preferred_video_codec)
        .or_else(|| ffmpeg::encoder::find(container_video_codec))
        .ok_or_else(|| {
            ScreenRecorderError::Export(format!(
                "no video encoder available for {format:?} (preferred={preferred_video_codec:?}, container={container_video_codec:?})"
            ))
        })?;
    let codec_video_info = video_codec.video().map_err(|err| {
        ScreenRecorderError::Export(format!(
            "selected video codec is not usable as video: {err}"
        ))
    })?;
    let pixel_format = choose_video_pixel_format(format, codec_video_info);
    let mut video_encoder = ffmpeg::codec::context::Context::new_with_codec(video_codec)
        .encoder()
        .video()
        .map_err(|err| {
            ScreenRecorderError::Export(format!("failed to create video encoder context: {err}"))
        })?;
    video_encoder.set_width(width);
    video_encoder.set_height(height);
    video_encoder.set_format(pixel_format);
    video_encoder.set_time_base(video_time_base);
    video_encoder.set_frame_rate(Some(video_frame_rate));
    if format != ExportFormat::Gif {
        video_encoder.set_bit_rate(smart_quality_bitrate_bps(
            width,
            height,
            export_fps,
            video_config,
            false,
        ));
    }
    if global_header {
        video_encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
    }
    let use_h264_options =
        video_codec.id() == ffmpeg::codec::Id::H264 || video_codec.name().contains("264");
    let mut video_encoder = if use_h264_options {
        let mut options = ffmpeg::Dictionary::new();
        options.set("preset", video_config.speed.as_x264_preset());
        options.set(
            "crf",
            &quality_to_h264_crf(video_config.quality).to_string(),
        );
        video_encoder
            .open_as_with(video_codec, options)
            .map_err(|err| {
                ScreenRecorderError::Export(format!(
                    "failed to open video encoder with h264 options: {err}"
                ))
            })?
    } else {
        video_encoder.open_as(video_codec).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to open video encoder: {err}"))
        })?
    };

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

    let mut audio_state = None;
    if let Some(mixed) = mixed_audio
        && format != ExportFormat::Gif
    {
        let container_audio_codec = output
            .format()
            .codec(output_path, ffmpeg::media::Type::Audio);
        let audio_codec = choose_audio_codec(format)
            .or_else(|| ffmpeg::encoder::find(container_audio_codec))
            .ok_or_else(|| {
                ScreenRecorderError::Export(format!(
                    "no audio encoder available for {format:?} (container={container_audio_codec:?})"
                ))
            })?;
        let codec_audio_info = audio_codec.audio().map_err(|err| {
            ScreenRecorderError::Export(format!(
                "selected audio codec is not usable as audio: {err}"
            ))
        })?;
        let output_rate_hz = choose_audio_sample_rate(codec_audio_info, mixed.sample_rate_hz);
        let output_layout = choose_audio_channel_layout(codec_audio_info, mixed.channels);
        let output_sample_format = choose_audio_sample_format(codec_audio_info);

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
        audio_encoder.set_bit_rate(usize::from(audio_bitrate_kbps.max(8)) * 1000);
        audio_encoder.set_time_base((1, output_rate_hz as i32));
        if global_header {
            audio_encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }

        let audio_encoder = if audio_codec.name().eq_ignore_ascii_case("aac") {
            let mut options = ffmpeg::Dictionary::new();
            options.set("profile", "aac_low");
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

        let audio_stream_index = {
            let mut stream = output.add_stream(audio_codec).map_err(|err| {
                ScreenRecorderError::Export(format!("failed to add output audio stream: {err}"))
            })?;
            stream.set_time_base(ffmpeg::Rational(1, output_rate_hz as i32));
            stream.set_rate(ffmpeg::Rational(output_rate_hz as i32, 1));
            stream.set_parameters(&audio_encoder);
            stream.index()
        };

        audio_state = Some((
            audio_encoder,
            audio_stream_index,
            ffmpeg::Rational(1, output_rate_hz as i32),
        ));
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

    if let Some((_, stream_index, stream_time_base)) = audio_state.as_mut() {
        *stream_time_base = output
            .stream(*stream_index)
            .map(|stream| stream.time_base())
            .ok_or_else(|| {
                ScreenRecorderError::Export(format!(
                    "failed to resolve output audio stream {} after header",
                    stream_index
                ))
            })?;
    }

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

        video_encoder.send_frame(&encode_frame).map_err(|err| {
            ScreenRecorderError::Export(format!("failed to send frame to video encoder: {err}"))
        })?;
        drain_video_packets(
            &mut video_encoder,
            &mut output,
            video_stream_index,
            video_stream_time_base,
            false,
        )?;
    }

    video_encoder.send_eof().map_err(|err| {
        ScreenRecorderError::Export(format!("failed to finalize video encoder: {err}"))
    })?;
    drain_video_packets(
        &mut video_encoder,
        &mut output,
        video_stream_index,
        video_stream_time_base,
        true,
    )?;

    if let (Some(audio), Some((audio_encoder, stream_index, stream_time_base))) =
        (mixed_audio, audio_state.as_mut())
    {
        encode_audio_samples(
            &mut output,
            audio_encoder,
            *stream_index,
            *stream_time_base,
            audio,
        )?;
    }

    output.write_trailer().map_err(|err| {
        ScreenRecorderError::Export(format!("failed to write output trailer: {err}"))
    })?;
    Ok(())
}

fn validate_export_frames(frames: &[StoredFrame], require_even: bool) -> Result<(u32, u32)> {
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

    if require_even && (width % 2 != 0 || height % 2 != 0) {
        return Err(ScreenRecorderError::Export(
            "selected export requires even width and height".to_string(),
        ));
    }

    let expected_len = width as usize * height as usize * 4;
    for frame in frames {
        if frame.width != width || frame.height != height {
            return Err(ScreenRecorderError::Export(format!(
                "dynamic resolution is unsupported (expected {}x{}, got {}x{})",
                width, height, frame.width, frame.height
            )));
        }
        if frame.rgba.len() != expected_len {
            return Err(ScreenRecorderError::Export(
                "RGBA input size mismatch".to_string(),
            ));
        }
    }

    Ok((width, height))
}

fn drain_video_packets(
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
                    "failed to receive encoded video packet: {err}"
                )));
            }
        }
    }
    Ok(())
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

fn encode_audio_samples(
    output: &mut ffmpeg::format::context::Output,
    encoder: &mut ffmpeg::encoder::audio::Encoder,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    mixed: &MixedAudio,
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
        if fs == 0 { 1024 } else { fs }
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

        if !variable_frame_size && chunk.len() < samples_per_chunk {
            chunk.resize(samples_per_chunk, 0);
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

    encoder.send_eof().map_err(|err| {
        ScreenRecorderError::Export(format!("failed to finalize audio encoder: {err}"))
    })?;
    drain_audio_packets(encoder, output, stream_index, stream_time_base, true)
}

fn validate_manifest_paths(manifest: &SessionManifest) -> Result<()> {
    validate_file_exists(&manifest.video_temp_path)?;
    validate_file_exists(&manifest.mouse_path)?;

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
    use std::path::PathBuf;

    fn test_editing_session(
        recorded_system_audio: bool,
        recorded_microphone_audio: bool,
    ) -> EditingSession {
        EditingSession {
            artifact: RecordingArtifact {
                session_id: "session".to_string(),
                output_dir: PathBuf::from("recordings"),
                temp_dir: PathBuf::from("recordings/tmp"),
                manifest_path: PathBuf::from("recordings/manifest.json"),
                recorded_system_audio,
                recorded_microphone_audio,
            },
            manifest: SessionManifest {
                session_id: "session".to_string(),
                output_dir: PathBuf::from("recordings"),
                temp_dir: PathBuf::from("recordings/tmp"),
                keep_temp_files: false,
                video_temp_path: PathBuf::from("recordings/tmp/video_recording.mp4"),
                audio_system_path: Some(PathBuf::from("recordings/tmp/system.pcm")),
                audio_mic_path: Some(PathBuf::from("recordings/tmp/mic.pcm")),
                mouse_path: PathBuf::from("recordings/tmp/mouse.jsonl"),
                fps: 30,
                recording_video_format: crate::config::RecordingVideoFormat::H264Lossless,
                recording_video: VideoEncodeConfig::default(),
                width: 1920,
                height: 1080,
                capture_origin_x: 0,
                capture_origin_y: 0,
                recorded_system_audio,
                recorded_microphone_audio,
                audio_sample_rate_hz: 48_000,
                audio_channels: 2,
                audio_bitrate_kbps: 192,
                pause_intervals: Vec::new(),
            },
            config: EditConfig::default(),
        }
    }

    #[test]
    fn set_config_disables_unrecorded_audio_tracks() {
        let mut editing = test_editing_session(false, false);
        let mut config = EditConfig::default();
        config.system_audio.enabled = true;
        config.microphone_audio.enabled = true;

        editing
            .set_config(config)
            .expect("set_config should gracefully disable unavailable audio tracks");

        assert!(!editing.config.system_audio.enabled);
        assert!(!editing.config.microphone_audio.enabled);
    }

    #[test]
    fn set_config_keeps_recorded_audio_tracks_enabled() {
        let mut editing = test_editing_session(true, true);
        let mut config = EditConfig::default();
        config.system_audio.enabled = true;
        config.microphone_audio.enabled = true;

        editing
            .set_config(config)
            .expect("set_config should keep recorded audio tracks enabled");

        assert!(editing.config.system_audio.enabled);
        assert!(editing.config.microphone_audio.enabled);
    }

    #[test]
    fn choose_export_fps_keeps_recording_fps() {
        assert_eq!(choose_export_fps(60, ExportFormat::Mp4), 60);
        assert_eq!(choose_export_fps(30, ExportFormat::Avi), 30);
    }

    #[test]
    fn choose_export_fps_clamps_gif() {
        assert_eq!(choose_export_fps(60, ExportFormat::Gif), 20);
        assert_eq!(choose_export_fps(10, ExportFormat::Gif), 10);
    }

    #[test]
    fn align_audio_pads_or_truncates_to_duration() {
        let padded =
            align_i16_interleaved_to_duration(vec![1; 20], 10, 2, Duration::from_millis(1_500));
        assert_eq!(padded.len(), 30);

        let truncated =
            align_i16_interleaved_to_duration(vec![1; 40], 10, 2, Duration::from_millis(1_500));
        assert_eq!(truncated.len(), 30);
    }

    #[test]
    fn build_mouse_tracks_keeps_shape_binding() {
        let store = MouseStore {
            schema_version: crate::mouse::MOUSE_STORE_SCHEMA_VERSION,
            cursor_shapes: vec![CursorShapeRecord {
                shape_id: 7,
                hotspot_x: 3,
                hotspot_y: 4,
                width: 8,
                height: 8,
                mode: CursorShapeCompositionMode::MaskedColor,
                shape_rgba: vec![255; 8 * 8 * 4],
            }],
            cursor_frames: vec![crate::mouse::CursorFrameRecord {
                timestamp_ms: 12,
                x: 100,
                y: 200,
                visible: true,
                shape_id: Some(7),
            }],
            clicks: vec![],
        };

        let tracks = build_mouse_tracks(&store);
        assert_eq!(tracks.samples.len(), 1);
        assert_eq!(tracks.samples[0].shape_id, Some(7));
        assert!(tracks.cursor_shapes.contains_key(&7));
        assert_eq!(
            tracks.cursor_shapes.get(&7).map(|s| s.mode),
            Some(CursorShapeCompositionMode::MaskedColor)
        );
    }

    #[test]
    fn apply_mouse_overlays_draws_cursor_shape_pixels() {
        let mut frame = StoredFrame {
            timestamp_ms: 0,
            duration_ms: 16,
            width: 4,
            height: 4,
            rgba: vec![0; 4 * 4 * 4],
        };
        let store = MouseStore {
            schema_version: crate::mouse::MOUSE_STORE_SCHEMA_VERSION,
            cursor_shapes: vec![CursorShapeRecord {
                shape_id: 1,
                hotspot_x: 0,
                hotspot_y: 0,
                width: 1,
                height: 1,
                mode: CursorShapeCompositionMode::AlphaBlend,
                shape_rgba: vec![200, 10, 20, 255],
            }],
            cursor_frames: vec![crate::mouse::CursorFrameRecord {
                timestamp_ms: 0,
                x: 2,
                y: 1,
                visible: true,
                shape_id: Some(1),
            }],
            clicks: vec![],
        };
        let tracks = build_mouse_tracks(&store);

        apply_mouse_overlays(
            &mut frame,
            &tracks,
            &MouseEditConfig {
                visible: true,
                trail_enabled: false,
                click_enabled: false,
                ..MouseEditConfig::default()
            },
        );

        let px = (1usize * 4 + 2usize) * 4;
        assert_eq!(&frame.rgba[px..px + 4], &[200, 10, 20, 255]);
    }

    #[test]
    fn apply_mouse_overlays_uses_fallback_for_fully_transparent_alpha_shape() {
        let mut frame = StoredFrame {
            timestamp_ms: 0,
            duration_ms: 16,
            width: 16,
            height: 16,
            rgba: vec![0; 16 * 16 * 4],
        };
        let store = MouseStore {
            schema_version: crate::mouse::MOUSE_STORE_SCHEMA_VERSION,
            cursor_shapes: vec![CursorShapeRecord {
                shape_id: 99,
                hotspot_x: 0,
                hotspot_y: 0,
                width: 2,
                height: 2,
                mode: CursorShapeCompositionMode::AlphaBlend,
                shape_rgba: vec![
                    255, 255, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0,
                ],
            }],
            cursor_frames: vec![crate::mouse::CursorFrameRecord {
                timestamp_ms: 0,
                x: 8,
                y: 8,
                visible: true,
                shape_id: Some(99),
            }],
            clicks: vec![],
        };
        let tracks = build_mouse_tracks(&store);

        apply_mouse_overlays(
            &mut frame,
            &tracks,
            &MouseEditConfig {
                visible: true,
                trail_enabled: false,
                click_enabled: false,
                ..MouseEditConfig::default()
            },
        );

        let cursor_pixel = (8usize * 16 + 8usize) * 4;
        assert!(
            frame.rgba[cursor_pixel + 3] > 0,
            "fallback cursor should draw even when sampled shape is fully transparent"
        );
    }

    #[test]
    fn apply_mouse_overlays_renders_masked_color_shape_without_black_box() {
        let mut frame = StoredFrame {
            timestamp_ms: 0,
            duration_ms: 16,
            width: 4,
            height: 2,
            rgba: vec![
                10, 20, 30, 255, 20, 40, 60, 255, 100, 120, 140, 255, 0, 0, 0, 255, // row 1
                0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255,
            ],
        };
        let store = MouseStore {
            schema_version: crate::mouse::MOUSE_STORE_SCHEMA_VERSION,
            cursor_shapes: vec![CursorShapeRecord {
                shape_id: 5,
                hotspot_x: 0,
                hotspot_y: 0,
                width: 3,
                height: 1,
                mode: CursorShapeCompositionMode::MaskedColor,
                shape_rgba: vec![
                    0, 0, 0, 0xFF, // alpha=0xFF + non-zero mask => XOR
                    0xFF, 0xFF, 0xFF, 0xFF, // alpha=0x00 => source copy
                    5, 6, 7, 0x00,
                ],
            }],
            cursor_frames: vec![crate::mouse::CursorFrameRecord {
                timestamp_ms: 0,
                x: 0,
                y: 0,
                visible: true,
                shape_id: Some(5),
            }],
            clicks: vec![],
        };
        let tracks = build_mouse_tracks(&store);

        apply_mouse_overlays(
            &mut frame,
            &tracks,
            &MouseEditConfig {
                visible: true,
                trail_enabled: false,
                click_enabled: false,
                ..MouseEditConfig::default()
            },
        );

        assert_eq!(&frame.rgba[0..4], &[10, 20, 30, 255]);
        assert_eq!(&frame.rgba[4..8], &[235, 215, 195, 255]);
        assert_eq!(&frame.rgba[8..12], &[5, 6, 7, 255]);
    }

    #[test]
    fn apply_mouse_overlays_uses_fallback_for_noop_masked_shape() {
        let mut frame = StoredFrame {
            timestamp_ms: 0,
            duration_ms: 16,
            width: 16,
            height: 16,
            rgba: vec![0; 16 * 16 * 4],
        };
        let store = MouseStore {
            schema_version: crate::mouse::MOUSE_STORE_SCHEMA_VERSION,
            cursor_shapes: vec![CursorShapeRecord {
                shape_id: 123,
                hotspot_x: 0,
                hotspot_y: 0,
                width: 2,
                height: 2,
                mode: CursorShapeCompositionMode::MaskedColor,
                shape_rgba: vec![0, 0, 0, 0xFF, 0, 0, 0, 0xFF, 0, 0, 0, 0xFF, 0, 0, 0, 0xFF],
            }],
            cursor_frames: vec![crate::mouse::CursorFrameRecord {
                timestamp_ms: 0,
                x: 8,
                y: 8,
                visible: true,
                shape_id: Some(123),
            }],
            clicks: vec![],
        };
        let tracks = build_mouse_tracks(&store);

        apply_mouse_overlays(
            &mut frame,
            &tracks,
            &MouseEditConfig {
                visible: true,
                trail_enabled: false,
                click_enabled: false,
                ..MouseEditConfig::default()
            },
        );

        let cursor_pixel = (8usize * 16 + 8usize) * 4;
        assert!(
            frame.rgba[cursor_pixel + 3] > 0,
            "fallback cursor should draw when masked shape pixels are all no-op"
        );
    }

    #[test]
    fn apply_mouse_overlays_trail_ignores_hidden_cursor_samples() {
        let mut frame = StoredFrame {
            timestamp_ms: 25,
            duration_ms: 16,
            width: 32,
            height: 32,
            rgba: vec![0; 32 * 32 * 4],
        };
        let store = MouseStore {
            schema_version: crate::mouse::MOUSE_STORE_SCHEMA_VERSION,
            cursor_shapes: vec![],
            cursor_frames: vec![
                crate::mouse::CursorFrameRecord {
                    timestamp_ms: 0,
                    x: 10,
                    y: 10,
                    visible: true,
                    shape_id: None,
                },
                crate::mouse::CursorFrameRecord {
                    timestamp_ms: 10,
                    x: 0,
                    y: 0,
                    visible: false,
                    shape_id: None,
                },
                crate::mouse::CursorFrameRecord {
                    timestamp_ms: 20,
                    x: 20,
                    y: 20,
                    visible: true,
                    shape_id: None,
                },
            ],
            clicks: vec![],
        };
        let tracks = build_mouse_tracks(&store);

        apply_mouse_overlays(
            &mut frame,
            &tracks,
            &MouseEditConfig {
                visible: false,
                trail_enabled: true,
                click_enabled: false,
                ..MouseEditConfig::default()
            },
        );

        let top_left = (1usize * 32 + 1usize) * 4;
        assert_eq!(&frame.rgba[top_left..top_left + 4], &[0, 0, 0, 0]);

        let mid = (15usize * 32 + 15usize) * 4;
        assert!(
            frame.rgba[mid] > 0 || frame.rgba[mid + 1] > 0 || frame.rgba[mid + 2] > 0,
            "visible cursor samples should still render trail segments"
        );
    }

    #[test]
    fn build_smoothed_trail_points_rounds_corners() {
        let points = vec![
            TrailCurvePoint {
                ts_ms: 0.0,
                x: 8.0,
                y: 8.0,
            },
            TrailCurvePoint {
                ts_ms: 10.0,
                x: 8.0,
                y: 24.0,
            },
            TrailCurvePoint {
                ts_ms: 20.0,
                x: 24.0,
                y: 24.0,
            },
        ];
        let smoothed = build_smoothed_trail_points(&points, 2.0);

        assert!(
            smoothed.len() > points.len(),
            "curve sampling should emit intermediate points"
        );
        assert!(
            smoothed
                .iter()
                .any(|p| p.x > 8.0 && p.x < 16.0 && p.y > 16.0 && p.y < 24.0),
            "smoothed trail should include rounded corner points between line segments"
        );
    }

    #[test]
    fn collect_visible_trail_window_moves_tail_continuously() {
        let samples = vec![
            MouseSample {
                ts_ms: 0,
                x: 0,
                y: 0,
                visible: true,
                shape_id: None,
            },
            MouseSample {
                ts_ms: 100,
                x: 100,
                y: 0,
                visible: true,
                shape_id: None,
            },
            MouseSample {
                ts_ms: 200,
                x: 200,
                y: 0,
                visible: true,
                shape_id: None,
            },
        ];

        let window_99 = collect_visible_trail_window(&samples, 99);
        let window_100 = collect_visible_trail_window(&samples, 100);
        let window_101 = collect_visible_trail_window(&samples, 101);

        assert!(!window_99.is_empty());
        assert!(!window_100.is_empty());
        assert!(!window_101.is_empty());
        assert!((window_99[0].x - 99.0).abs() < 0.01);
        assert!((window_100[0].x - 100.0).abs() < 0.01);
        assert!((window_101[0].x - 101.0).abs() < 0.01);
    }
}
