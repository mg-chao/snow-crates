use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next as ffmpeg;
use snow_audio_recorder::{
    AudioEvent, AudioFormat, AudioPacket, AudioSampleFormat, AudioSession, AudioSourceKind,
    AudioStreamConfig, AudioTimestampAnchor, DeviceSelector, SourceConfig, align_packet_frames,
};
use snow_capture::{
    CaptureEvent, CaptureMode, CaptureSession, CaptureTarget, CursorCompositionMode, StreamConfig,
};
use uuid::Uuid;

use crate::artifact::{RecordingArtifact, SessionManifest};
use crate::config::{RecordingConfig, RecordingTarget, RecordingVideoFormat, VideoEncodeConfig};
use crate::error::{Result, ScreenRecorderError};
use crate::mouse::{
    CursorFrameRecord, CursorShapeCompositionMode, CursorShapeRecord, MouseStore,
    write_mouse_records,
};
use crate::temp::TempLayout;
use crate::timeline::PauseTimeline;
use crate::video_quality::{quality_to_h264_crf, smart_quality_bitrate_bps};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingState {
    Created,
    Running,
    Paused,
    Stopped,
}

impl RecordingState {
    fn as_u8(self) -> u8 {
        match self {
            Self::Created => 0,
            Self::Running => 1,
            Self::Paused => 2,
            Self::Stopped => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Running,
            2 => Self::Paused,
            3 => Self::Stopped,
            _ => Self::Created,
        }
    }
}

enum WorkerCommand {
    Pause,
    Resume,
    Stop,
}

struct RuntimeHandles {
    control_tx: Sender<WorkerCommand>,
    worker_handle: JoinHandle<Result<WorkerOutcome>>,
}

#[derive(Debug)]
struct WorkerOutcome {
    width: u32,
    height: u32,
    pause_intervals: Vec<crate::artifact::PauseInterval>,
    recorded_system_audio: bool,
    recorded_microphone_audio: bool,
}

pub struct RecordingSession {
    config: RecordingConfig,
    session_id: String,
    layout: TempLayout,
    state: AtomicU8,
    runtime: Mutex<Option<RuntimeHandles>>,
    capture_origin: Mutex<(i32, i32)>,
}

impl RecordingSession {
    pub fn create(config: RecordingConfig) -> Result<Self> {
        config
            .validate()
            .map_err(ScreenRecorderError::InvalidConfig)?;

        let session_id = Uuid::new_v4().simple().to_string();
        let layout = TempLayout::create(&config, &session_id)?;

        Ok(Self {
            config,
            session_id,
            layout,
            state: AtomicU8::new(RecordingState::Created.as_u8()),
            runtime: Mutex::new(None),
            capture_origin: Mutex::new((0, 0)),
        })
    }

    pub fn start(&mut self) -> Result<()> {
        if self.state() != RecordingState::Created {
            return Err(ScreenRecorderError::InvalidConfig(
                "recording session can only be started from Created state".to_string(),
            ));
        }

        let capture_target = recording_target_to_capture_target(&self.config.target);
        let origin = resolve_capture_origin(&self.config.target)?;
        {
            let mut guard = self.capture_origin.lock().map_err(|_| {
                ScreenRecorderError::InvalidConfig("capture_origin lock poisoned".to_string())
            })?;
            *guard = origin;
        }

        let capture_session = CaptureSession::builder()
            .capture_mode(CaptureMode::ScreenRecording)
            .capture_cursor(true)
            .build()?;

        let started_at = Instant::now();
        let mut audio_stream = start_audio_stream_if_enabled(&self.config)?;
        let capture_stream = match capture_session.start_streaming(
            capture_target,
            StreamConfig {
                target_fps: self.config.fps,
                buffer_depth: 8,
                max_consecutive_errors: 30,
                adaptive_fps: true,
                min_fps: 10,
                pause_on_resolution_change: false,
            },
        ) {
            Ok(stream) => stream,
            Err(err) => {
                if let Some(audio) = audio_stream.take() {
                    audio.stop();
                    let _ = audio.stop_and_drain();
                }
                return Err(ScreenRecorderError::Capture(err));
            }
        };

        let (control_tx, control_rx) = crossbeam_channel::unbounded::<WorkerCommand>();
        let layout = self.layout.clone();
        let config = self.config.clone();
        let worker_handle = std::thread::Builder::new()
            .name("snow-screen-recorder-worker".to_string())
            .spawn(move || {
                recording_worker(
                    config,
                    layout,
                    capture_stream,
                    audio_stream,
                    control_rx,
                    started_at,
                    origin.0,
                    origin.1,
                )
            })
            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;

        let runtime = RuntimeHandles {
            control_tx,
            worker_handle,
        };

        let mut guard = self
            .runtime
            .lock()
            .map_err(|_| ScreenRecorderError::InvalidConfig("runtime lock poisoned".to_string()))?;
        *guard = Some(runtime);
        self.state
            .store(RecordingState::Running.as_u8(), Ordering::Release);
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        if self.state() != RecordingState::Running {
            return Err(ScreenRecorderError::InvalidConfig(
                "pause is only allowed while recording is Running".to_string(),
            ));
        }

        let guard = self
            .runtime
            .lock()
            .map_err(|_| ScreenRecorderError::InvalidConfig("runtime lock poisoned".to_string()))?;
        let runtime = guard.as_ref().ok_or_else(|| {
            ScreenRecorderError::InvalidConfig("recording runtime is not initialized".to_string())
        })?;
        runtime
            .control_tx
            .send(WorkerCommand::Pause)
            .map_err(|_| ScreenRecorderError::Encode("recording worker has stopped".to_string()))?;
        self.state
            .store(RecordingState::Paused.as_u8(), Ordering::Release);
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        if self.state() != RecordingState::Paused {
            return Err(ScreenRecorderError::InvalidConfig(
                "resume is only allowed while recording is Paused".to_string(),
            ));
        }

        let guard = self
            .runtime
            .lock()
            .map_err(|_| ScreenRecorderError::InvalidConfig("runtime lock poisoned".to_string()))?;
        let runtime = guard.as_ref().ok_or_else(|| {
            ScreenRecorderError::InvalidConfig("recording runtime is not initialized".to_string())
        })?;
        runtime
            .control_tx
            .send(WorkerCommand::Resume)
            .map_err(|_| ScreenRecorderError::Encode("recording worker has stopped".to_string()))?;
        self.state
            .store(RecordingState::Running.as_u8(), Ordering::Release);
        Ok(())
    }

    pub fn stop(self) -> Result<RecordingArtifact> {
        let mut runtime_guard = self
            .runtime
            .lock()
            .map_err(|_| ScreenRecorderError::InvalidConfig("runtime lock poisoned".to_string()))?;
        let runtime = runtime_guard.take().ok_or_else(|| {
            ScreenRecorderError::InvalidConfig("recording session was not started".to_string())
        })?;
        drop(runtime_guard);

        let _ = runtime.control_tx.send(WorkerCommand::Stop);

        let worker_result = runtime.worker_handle.join().map_err(|_| {
            ScreenRecorderError::Encode("recording worker thread panicked".to_string())
        })?;

        self.state
            .store(RecordingState::Stopped.as_u8(), Ordering::Release);

        let outcome = worker_result?;

        let (capture_origin_x, capture_origin_y) = *self.capture_origin.lock().map_err(|_| {
            ScreenRecorderError::InvalidConfig("capture_origin lock poisoned".to_string())
        })?;

        let manifest = SessionManifest {
            session_id: self.session_id.clone(),
            output_dir: self.layout.output_dir.clone(),
            temp_dir: self.layout.session_dir.clone(),
            keep_temp_files: self.config.keep_temp_files,
            video_temp_path: self.layout.video_temp_path.clone(),
            audio_system_path: self
                .config
                .audio
                .system_audio_enabled
                .then(|| self.layout.audio_system_path.clone()),
            audio_mic_path: self
                .config
                .audio
                .microphone_enabled
                .then(|| self.layout.audio_mic_path.clone()),
            mouse_path: self.layout.mouse_path.clone(),
            fps: self.config.fps,
            recording_video_format: self.config.video_format,
            recording_video: self.config.video.clone(),
            width: outcome.width,
            height: outcome.height,
            capture_origin_x,
            capture_origin_y,
            recorded_system_audio: outcome.recorded_system_audio,
            recorded_microphone_audio: outcome.recorded_microphone_audio,
            audio_sample_rate_hz: self.config.audio.sample_rate_hz,
            audio_channels: self.config.audio.channels.channels(),
            audio_bitrate_kbps: self.config.audio.bitrate_kbps,
            pause_intervals: outcome.pause_intervals,
        };
        manifest.write_to_path(&self.layout.manifest_path)?;

        Ok(RecordingArtifact {
            session_id: self.session_id,
            output_dir: self.layout.output_dir,
            temp_dir: self.layout.session_dir,
            manifest_path: self.layout.manifest_path,
            recorded_system_audio: manifest.recorded_system_audio,
            recorded_microphone_audio: manifest.recorded_microphone_audio,
        })
    }

    pub fn state(&self) -> RecordingState {
        RecordingState::from_u8(self.state.load(Ordering::Acquire))
    }
}

fn recording_target_to_capture_target(target: &RecordingTarget) -> CaptureTarget {
    match target {
        RecordingTarget::PrimaryMonitor => CaptureTarget::PrimaryMonitor,
        RecordingTarget::Monitor(id) => CaptureTarget::Monitor(id.clone()),
        RecordingTarget::Window(id) => CaptureTarget::Window(*id),
        RecordingTarget::Region(region) => CaptureTarget::Region(*region),
    }
}

fn start_audio_stream_if_enabled(
    config: &RecordingConfig,
) -> Result<Option<snow_audio_recorder::AudioStreamHandle>> {
    if !config.audio.system_audio_enabled && !config.audio.microphone_enabled {
        return Ok(None);
    }

    let channels = config.audio.channels.channels();
    let format = AudioFormat::new(
        config.audio.sample_rate_hz,
        channels,
        AudioSampleFormat::I16,
    );
    let packet_duration = Duration::from_millis(20);

    let mut stream_config = AudioStreamConfig::default();
    stream_config.system = SourceConfig {
        enabled: config.audio.system_audio_enabled,
        required: config.audio.system_audio_enabled,
        device: DeviceSelector::DefaultRender,
        output_format: format,
        packet_duration,
    };
    stream_config.microphone = SourceConfig {
        enabled: config.audio.microphone_enabled,
        required: config.audio.microphone_enabled,
        device: config
            .audio
            .microphone_device
            .clone()
            .unwrap_or(DeviceSelector::DefaultCapture),
        output_format: format,
        packet_duration,
    };

    let session = AudioSession::new()?;
    let stream = session.start_streaming(stream_config)?;
    Ok(Some(stream))
}

fn resolve_capture_origin(target: &RecordingTarget) -> Result<(i32, i32)> {
    match target {
        RecordingTarget::Region(region) => Ok((region.x, region.y)),
        RecordingTarget::Window(window) => {
            #[cfg(target_os = "windows")]
            {
                use windows::Win32::Foundation::{HWND, RECT};
                use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

                let hwnd = HWND(window.raw_handle() as *mut std::ffi::c_void);
                let mut rect = RECT::default();
                unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(|e| {
                    ScreenRecorderError::Capture(snow_capture::error::CaptureError::InvalidConfig(
                        format!("failed to resolve window bounds: {e}"),
                    ))
                })?;
                Ok((rect.left, rect.top))
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = window;
                Ok((0, 0))
            }
        }
        RecordingTarget::PrimaryMonitor | RecordingTarget::Monitor(_) => {
            let layout = snow_capture::MonitorLayout::snapshot()?;
            let monitor_geo = match target {
                RecordingTarget::PrimaryMonitor => layout
                    .monitors
                    .iter()
                    .find(|m| m.monitor.is_primary())
                    .ok_or_else(|| {
                        ScreenRecorderError::Capture(
                            snow_capture::error::CaptureError::InvalidTarget(
                                "primary monitor not found".to_string(),
                            ),
                        )
                    })?,
                RecordingTarget::Monitor(selected) => layout
                    .monitors
                    .iter()
                    .find(|m| m.monitor.stable_id() == selected.stable_id())
                    .ok_or_else(|| {
                        ScreenRecorderError::Capture(
                            snow_capture::error::CaptureError::InvalidTarget(format!(
                                "monitor {} not found",
                                selected.stable_id()
                            )),
                        )
                    })?,
                _ => unreachable!(),
            };
            Ok((monitor_geo.x, monitor_geo.y))
        }
    }
}

struct PcmTrackWriter {
    writer: BufWriter<File>,
    sample_rate_hz: u32,
    channels: u16,
    written_frames: u64,
    anchor: AudioTimestampAnchor,
}

impl PcmTrackWriter {
    fn create(
        path: &std::path::Path,
        sample_rate_hz: u32,
        channels: u16,
        started_at: Instant,
    ) -> Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            sample_rate_hz: sample_rate_hz.max(1),
            channels: channels.max(1),
            written_frames: 0,
            anchor: AudioTimestampAnchor::from_origin_instant(started_at),
        })
    }

    fn append_silence_frames(&mut self, frames: u64) -> Result<()> {
        if frames == 0 {
            return Ok(());
        }

        let channels = usize::from(self.channels);
        const CHUNK_FRAMES: u64 = 4096;
        let chunk = vec![0u8; CHUNK_FRAMES as usize * channels * 2];

        let mut remaining = frames;
        while remaining > 0 {
            let take = remaining.min(CHUNK_FRAMES);
            let take_bytes = take as usize * channels * 2;
            self.writer.write_all(&chunk[..take_bytes])?;
            self.written_frames = self.written_frames.saturating_add(take);
            remaining -= take;
        }

        Ok(())
    }

    fn append_i16_bytes(&mut self, bytes: &[u8]) -> Result<u64> {
        if bytes.is_empty() {
            return Ok(0);
        }

        let channels = usize::from(self.channels);
        if bytes.len() % (channels * 2) != 0 {
            return Err(ScreenRecorderError::Encode(
                "PCM bytes are not channel aligned".to_string(),
            ));
        }

        self.writer.write_all(bytes)?;
        let frames = (bytes.len() / 2 / channels) as u64;
        self.written_frames = self.written_frames.saturating_add(frames);
        Ok(frames)
    }

    fn write_packet(
        &mut self,
        packet: &AudioPacket,
        bytes: &[u8],
        timeline: &PauseTimeline,
    ) -> Result<u64> {
        let channels = usize::from(self.channels);
        if bytes.len() % (channels * 2) != 0 {
            return Err(ScreenRecorderError::Encode(
                "audio packet bytes are not channel aligned".to_string(),
            ));
        }

        let packet_frames = (bytes.len() / 2 / channels) as u64;
        if packet_frames == 0 {
            return Ok(0);
        }

        let packet_ts = self.anchor.stream_relative(packet);
        let active_start = timeline.active_elapsed_from_stream_offset(packet_ts.start);
        let aligned = align_packet_frames(
            self.written_frames,
            self.sample_rate_hz,
            snow_audio_recorder::AudioPacketTimestamp {
                start: active_start,
                end: timeline.active_elapsed_from_stream_offset(packet_ts.end),
            },
            packet_frames,
        );

        if aligned.silence_prefix_frames > 0 {
            self.append_silence_frames(aligned.silence_prefix_frames)?;
        }

        if aligned.write_packet_frames == 0 {
            return Ok(0);
        }

        let skip_samples = aligned
            .skip_packet_frames
            .checked_mul(channels as u64)
            .ok_or_else(|| {
                ScreenRecorderError::Encode("audio overlap computation overflow".to_string())
            })? as usize;
        let skip_bytes = skip_samples.checked_mul(2).ok_or_else(|| {
            ScreenRecorderError::Encode("audio overlap byte computation overflow".to_string())
        })?;

        self.append_i16_bytes(&bytes[skip_bytes..])
    }

    fn finish(mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

struct LiveVideoEncoder {
    output: ffmpeg::format::context::Output,
    encoder: ffmpeg::encoder::video::Encoder,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    scaler: ffmpeg::software::scaling::Context,
    rgba_frame: ffmpeg::frame::Video,
    encode_frame: ffmpeg::frame::Video,
    width: u32,
    height: u32,
    fps: u32,
    last_pts: Option<i64>,
}

impl LiveVideoEncoder {
    fn create(
        path: &std::path::Path,
        width: u32,
        height: u32,
        fps: u32,
        video_format: RecordingVideoFormat,
        video_config: &VideoEncodeConfig,
    ) -> Result<Self> {
        ensure_ffmpeg_initialized()?;

        let mut output = ffmpeg::format::output(path).map_err(|err| {
            ScreenRecorderError::Encode(format!(
                "failed to create temporary video output {}: {err}",
                path.display()
            ))
        })?;
        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);
        let fps = fps.max(1).min(i32::MAX as u32);
        let video_time_base = ffmpeg::Rational(1, fps as i32);
        let video_frame_rate = ffmpeg::Rational(fps as i32, 1);

        let container_video_codec = output.format().codec(path, ffmpeg::media::Type::Video);
        let video_codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)
            .or_else(|| ffmpeg::encoder::find(container_video_codec))
            .ok_or_else(|| {
                ScreenRecorderError::Encode(
                    "no usable video encoder available for temporary recording file".to_string(),
                )
            })?;
        let codec_video_info = video_codec.video().map_err(|err| {
            ScreenRecorderError::Encode(format!(
                "selected temporary video codec is not a video encoder: {err}"
            ))
        })?;
        let pixel_format = choose_video_pixel_format(codec_video_info);

        let mut video_encoder = ffmpeg::codec::context::Context::new_with_codec(video_codec)
            .encoder()
            .video()
            .map_err(|err| {
                ScreenRecorderError::Encode(format!(
                    "failed to create temporary video encoder context: {err}"
                ))
            })?;
        video_encoder.set_width(width);
        video_encoder.set_height(height);
        video_encoder.set_format(pixel_format);
        video_encoder.set_time_base(video_time_base);
        video_encoder.set_frame_rate(Some(video_frame_rate));
        video_encoder.set_bit_rate(smart_quality_bitrate_bps(
            width,
            height,
            fps,
            video_config,
            matches!(video_format, RecordingVideoFormat::H264Lossless),
        ));
        if global_header {
            video_encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }

        let use_h264_options =
            video_codec.id() == ffmpeg::codec::Id::H264 || video_codec.name().contains("264");
        let video_encoder = if use_h264_options {
            let mut options = ffmpeg::Dictionary::new();
            options.set("preset", video_config.speed.as_x264_preset());
            match video_format {
                RecordingVideoFormat::H264Lossless => {
                    options.set("crf", "0");
                    options.set("qp", "0");
                }
                RecordingVideoFormat::H264 => {
                    options.set(
                        "crf",
                        &quality_to_h264_crf(video_config.quality).to_string(),
                    );
                }
            }
            video_encoder
                .open_as_with(video_codec, options)
                .map_err(|err| {
                    ScreenRecorderError::Encode(format!(
                        "failed to open temporary video encoder with h264 options: {err}"
                    ))
                })?
        } else {
            video_encoder.open_as(video_codec).map_err(|err| {
                ScreenRecorderError::Encode(format!(
                    "failed to open temporary video encoder: {err}"
                ))
            })?
        };

        let stream_index = {
            let mut stream = output.add_stream(video_codec).map_err(|err| {
                ScreenRecorderError::Encode(format!(
                    "failed to add temporary video output stream: {err}"
                ))
            })?;
            stream.set_time_base(video_time_base);
            stream.set_rate(video_frame_rate);
            stream.set_avg_frame_rate(video_frame_rate);
            stream.set_parameters(&video_encoder);
            stream.index()
        };

        output.write_header().map_err(|err| {
            ScreenRecorderError::Encode(format!(
                "failed to write temporary video output header: {err}"
            ))
        })?;
        let stream_time_base = output
            .stream(stream_index)
            .map(|stream| stream.time_base())
            .ok_or_else(|| {
                ScreenRecorderError::Encode(format!(
                    "failed to resolve temporary video stream {stream_index} after header"
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
            ScreenRecorderError::Encode(format!("failed to create temporary video scaler: {err}"))
        })?;

        Ok(Self {
            output,
            encoder: video_encoder,
            stream_index,
            stream_time_base,
            scaler,
            rgba_frame: ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, width, height),
            encode_frame: ffmpeg::frame::Video::new(pixel_format, width, height),
            width,
            height,
            fps,
            last_pts: None,
        })
    }

    fn encode_frame(&mut self, rgba: &[u8], timestamp_ms: u64) -> Result<()> {
        let pts = self
            .timestamp_to_pts(timestamp_ms)
            .max(self.last_pts.unwrap_or(-1).saturating_add(1));
        self.encode_frame_at_pts(rgba, pts)
    }

    fn finalize(mut self, final_timestamp_ms: u64, tail_rgba: Option<&[u8]>) -> Result<()> {
        if let (Some(last_pts), Some(rgba)) = (self.last_pts, tail_rgba) {
            let final_pts = self.timestamp_to_pts(final_timestamp_ms);
            if final_pts > last_pts {
                self.encode_frame_at_pts(rgba, final_pts)?;
            }
        }

        self.encoder.send_eof().map_err(|err| {
            ScreenRecorderError::Encode(format!(
                "failed to finalize temporary video encoder: {err}"
            ))
        })?;
        self.drain_packets(true)?;
        self.output.write_trailer().map_err(|err| {
            ScreenRecorderError::Encode(format!(
                "failed to write temporary video output trailer: {err}"
            ))
        })?;
        Ok(())
    }

    fn encode_frame_at_pts(&mut self, rgba: &[u8], pts: i64) -> Result<()> {
        let expected_len = self.width as usize * self.height as usize * 4;
        if rgba.len() != expected_len {
            return Err(ScreenRecorderError::Encode(format!(
                "temporary video RGBA size mismatch: expected {expected_len} bytes, got {}",
                rgba.len()
            )));
        }

        copy_rgba_into_frame(&mut self.rgba_frame, self.width, rgba);
        self.scaler
            .run(&self.rgba_frame, &mut self.encode_frame)
            .map_err(|err| {
                ScreenRecorderError::Encode(format!(
                    "failed to convert frame for temporary video encoding: {err}"
                ))
            })?;
        self.encode_frame.set_pts(Some(pts));

        self.encoder.send_frame(&self.encode_frame).map_err(|err| {
            ScreenRecorderError::Encode(format!(
                "failed to send frame to temporary video encoder: {err}"
            ))
        })?;
        self.drain_packets(false)?;
        self.last_pts = Some(pts);
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
                        ScreenRecorderError::Encode(format!(
                            "failed to write temporary encoded video packet: {err}"
                        ))
                    })?;
                }
                Err(err) if err == ffmpeg::Error::Eof => break,
                Err(err) if is_eagain(&err) && !draining => break,
                Err(err) if is_eagain(&err) && draining => continue,
                Err(err) => {
                    return Err(ScreenRecorderError::Encode(format!(
                        "failed to receive temporary encoded video packet: {err}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn timestamp_to_pts(&self, timestamp_ms: u64) -> i64 {
        let pts = (u128::from(timestamp_ms) * u128::from(self.fps) + 500) / 1_000;
        pts.min(i64::MAX as u128) as i64
    }
}

fn ensure_ffmpeg_initialized() -> Result<()> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| ffmpeg::init().map_err(|err| err.to_string()))
        .clone()
        .map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to initialize ffmpeg for recording: {err}"))
        })
}

fn is_eagain(err: &ffmpeg::Error) -> bool {
    matches!(
        err,
        ffmpeg::Error::Other { errno } if *errno == ffmpeg::error::EAGAIN
    )
}

fn choose_video_pixel_format(codec: ffmpeg::codec::Video) -> ffmpeg::format::Pixel {
    let preferred = [
        ffmpeg::format::Pixel::YUV420P,
        ffmpeg::format::Pixel::YUV422P,
        ffmpeg::format::Pixel::RGB24,
    ];
    if let Some(formats) = codec.formats() {
        let available: Vec<_> = formats.collect();
        for pixel in preferred {
            if available.iter().any(|fmt| *fmt == pixel) {
                return pixel;
            }
        }
        if let Some(first) = available.first().copied() {
            return first;
        }
    }
    ffmpeg::format::Pixel::YUV420P
}

fn copy_rgba_into_frame(frame: &mut ffmpeg::frame::Video, width: u32, rgba: &[u8]) {
    let stride = frame.stride(0);
    let row_bytes = width as usize * 4;
    let height = frame.height() as usize;
    let dst = frame.data_mut(0);

    for y in 0..height {
        let src_start = y * row_bytes;
        let src_end = src_start + row_bytes;
        let dst_start = y * stride;
        let dst_end = dst_start + row_bytes;
        dst[dst_start..dst_end].copy_from_slice(&rgba[src_start..src_end]);
    }
}

struct WorkerContext {
    layout: TempLayout,
    frame_interval_ms: u32,
    target_fps: u32,
    video_format: RecordingVideoFormat,
    video_config: VideoEncodeConfig,
    timeline: PauseTimeline,
    capture_origin_x: i32,
    capture_origin_y: i32,
    width: u32,
    height: u32,
    last_encoded_rgba: Option<Vec<u8>>,
    last_observed_ts_ms: Option<u64>,
    mouse_store: MouseStore,
    cursor_shapes_emitted: HashSet<u64>,
    last_cursor_frame: Option<CursorFrameRecord>,
    system_audio: Option<PcmTrackWriter>,
    mic_audio: Option<PcmTrackWriter>,
    preview_encoder: Option<LiveVideoEncoder>,
    capture_ended: bool,
    audio_ended: bool,
    recorded_system_audio: bool,
    recorded_microphone_audio: bool,
}

impl WorkerContext {
    fn new(
        config: &RecordingConfig,
        layout: TempLayout,
        started_at: Instant,
        capture_origin_x: i32,
        capture_origin_y: i32,
    ) -> Result<Self> {
        let frame_interval_ms = ((1000.0 / config.fps.max(1) as f32).round() as u32).max(1);
        let sample_rate_hz = config.audio.sample_rate_hz.max(1);
        let channels = config.audio.channels.channels().max(1);

        let system_audio = if config.audio.system_audio_enabled {
            Some(PcmTrackWriter::create(
                &layout.audio_system_path,
                sample_rate_hz,
                channels,
                started_at,
            )?)
        } else {
            None
        };

        let mic_audio = if config.audio.microphone_enabled {
            Some(PcmTrackWriter::create(
                &layout.audio_mic_path,
                sample_rate_hz,
                channels,
                started_at,
            )?)
        } else {
            None
        };

        Ok(Self {
            layout,
            frame_interval_ms,
            target_fps: config.fps.max(1),
            video_format: config.video_format,
            video_config: config.video.clone(),
            timeline: PauseTimeline::new(started_at),
            capture_origin_x,
            capture_origin_y,
            width: 0,
            height: 0,
            last_encoded_rgba: None,
            last_observed_ts_ms: None,
            mouse_store: MouseStore::new(),
            cursor_shapes_emitted: HashSet::new(),
            last_cursor_frame: None,
            system_audio,
            mic_audio,
            preview_encoder: None,
            capture_ended: false,
            audio_ended: false,
            recorded_system_audio: false,
            recorded_microphone_audio: false,
        })
    }

    fn pause(&mut self, at: Instant) {
        let ts = self.timeline.active_elapsed_ms(at);
        self.observe_video_time(ts);
        self.timeline.mark_pause(at);
    }

    fn resume(&mut self, at: Instant) {
        self.timeline.mark_resume(at);
    }

    fn observe_video_time(&mut self, ts_ms: u64) {
        self.last_observed_ts_ms = Some(ts_ms);
    }

    fn record_cursor_frame(&mut self, ts_ms: u64, cursor: &snow_capture::CursorData) {
        if let Some(shape) = cursor.shape.as_ref()
            && self.cursor_shapes_emitted.insert(shape.shape_id)
        {
            self.mouse_store.cursor_shapes.push(CursorShapeRecord {
                shape_id: shape.shape_id,
                hotspot_x: shape.hotspot_x,
                hotspot_y: shape.hotspot_y,
                width: shape.width,
                height: shape.height,
                mode: map_cursor_composition_mode(shape.composition_mode),
                shape_rgba: shape.shape_rgba.clone(),
            });
        }

        let frame = CursorFrameRecord {
            timestamp_ms: ts_ms,
            x: cursor.position_x - self.capture_origin_x,
            y: cursor.position_y - self.capture_origin_y,
            visible: cursor.visible,
            shape_id: cursor.shape_id,
        };
        self.last_cursor_frame = Some(frame.clone());
        self.mouse_store.cursor_frames.push(frame);
    }

    fn synthesize_cursor_frame_for_drop(&mut self, ts_ms: u64) {
        if let Some(mut last) = self.last_cursor_frame.clone() {
            last.timestamp_ms = ts_ms;
            self.last_cursor_frame = Some(last.clone());
            self.mouse_store.cursor_frames.push(last);
        }
    }

    fn handle_capture_event(&mut self, event: CaptureEvent) -> Result<()> {
        match event {
            CaptureEvent::Frame(frame) => self.handle_frame(frame),
            CaptureEvent::FrameDropped { .. } => {
                if let Some(last) = self.last_observed_ts_ms {
                    let next_ts = last.saturating_add(u64::from(self.frame_interval_ms));
                    self.observe_video_time(next_ts);
                    self.synthesize_cursor_frame_for_drop(next_ts);
                }
                Ok(())
            }
            CaptureEvent::Paused { at } => {
                self.pause(at);
                Ok(())
            }
            CaptureEvent::Resumed { at, .. } => {
                self.resume(at);
                Ok(())
            }
            CaptureEvent::ResolutionChanged { .. } => Err(ScreenRecorderError::Encode(
                "resolution changes during recording are not supported in this version".to_string(),
            )),
            CaptureEvent::StreamEnded => {
                self.capture_ended = true;
                Ok(())
            }
            CaptureEvent::Error(err) => Err(ScreenRecorderError::Capture(err)),
        }
    }

    fn handle_frame(&mut self, frame: snow_capture::frame::Frame) -> Result<()> {
        let (width, height) = frame.dimensions();
        if self.width == 0 || self.height == 0 {
            self.width = width;
            self.height = height;
        } else if self.width != width || self.height != height {
            return Err(ScreenRecorderError::Encode(format!(
                "dynamic resolution is unsupported (expected {}x{}, got {}x{})",
                self.width, self.height, width, height
            )));
        }

        let capture_at = frame.metadata.capture_time.unwrap_or_else(Instant::now);
        let ts_ms = self.timeline.active_elapsed_ms(capture_at);
        self.observe_video_time(ts_ms);

        if let Some(cursor) = frame.metadata.cursor.as_ref() {
            self.record_cursor_frame(ts_ms, cursor);
        }

        if frame.metadata.is_duplicate {
            return Ok(());
        }

        let rgba = frame.as_rgba_bytes().to_vec();

        if self.preview_encoder.is_none() {
            self.preview_encoder = Some(LiveVideoEncoder::create(
                &self.layout.video_temp_path,
                width,
                height,
                self.target_fps,
                self.video_format,
                &self.video_config,
            )?);
        }
        if let Some(encoder) = self.preview_encoder.as_mut() {
            encoder.encode_frame(&rgba, ts_ms)?;
        }
        self.last_encoded_rgba = Some(rgba);
        Ok(())
    }

    fn handle_audio_event(&mut self, event: AudioEvent) -> Result<()> {
        match event {
            AudioEvent::Packet(packet) => {
                let bytes = audio_packet_to_i16_le_bytes(&packet)?;
                if bytes.is_empty() {
                    return Ok(());
                }

                match packet.source {
                    AudioSourceKind::System => {
                        if let Some(writer) = self.system_audio.as_mut() {
                            let appended = writer.write_packet(&packet, &bytes, &self.timeline)?;
                            if appended > 0 {
                                self.recorded_system_audio = true;
                            }
                        }
                    }
                    AudioSourceKind::Microphone => {
                        if let Some(writer) = self.mic_audio.as_mut() {
                            let appended = writer.write_packet(&packet, &bytes, &self.timeline)?;
                            if appended > 0 {
                                self.recorded_microphone_audio = true;
                            }
                        }
                    }
                }
                Ok(())
            }
            AudioEvent::PacketDropped {
                source,
                dropped_frames,
            } => {
                match source {
                    AudioSourceKind::System => {
                        if let Some(writer) = self.system_audio.as_mut() {
                            writer.append_silence_frames(dropped_frames)?;
                            if dropped_frames > 0 {
                                self.recorded_system_audio = true;
                            }
                        }
                    }
                    AudioSourceKind::Microphone => {
                        if let Some(writer) = self.mic_audio.as_mut() {
                            writer.append_silence_frames(dropped_frames)?;
                            if dropped_frames > 0 {
                                self.recorded_microphone_audio = true;
                            }
                        }
                    }
                }
                Ok(())
            }
            AudioEvent::StreamEnded => {
                self.audio_ended = true;
                Ok(())
            }
            AudioEvent::Error(err) => Err(ScreenRecorderError::Audio(err)),
            _ => Ok(()),
        }
    }

    fn finalize(mut self, at: Instant) -> Result<WorkerOutcome> {
        self.timeline.finalize(at);
        let final_ts_ms = self.timeline.active_elapsed_ms(at);
        self.observe_video_time(final_ts_ms);

        if let Some(encoder) = self.preview_encoder.take() {
            let tail_rgba = self.last_encoded_rgba.as_deref();
            encoder.finalize(final_ts_ms, tail_rgba)?;
        }

        if self.last_encoded_rgba.is_none() {
            return Err(ScreenRecorderError::Encode(
                "recording ended without any video frames".to_string(),
            ));
        }

        write_mouse_records(&self.layout.mouse_path, &self.mouse_store)?;

        if let Some(writer) = self.system_audio.take() {
            writer.finish()?;
        }
        if let Some(writer) = self.mic_audio.take() {
            writer.finish()?;
        }

        Ok(WorkerOutcome {
            width: self.width,
            height: self.height,
            pause_intervals: self.timeline.intervals().to_vec(),
            recorded_system_audio: self.recorded_system_audio,
            recorded_microphone_audio: self.recorded_microphone_audio,
        })
    }
}

fn recording_worker(
    config: RecordingConfig,
    layout: TempLayout,
    capture_stream: snow_capture::StreamHandle,
    mut audio_stream: Option<snow_audio_recorder::AudioStreamHandle>,
    control_rx: Receiver<WorkerCommand>,
    started_at: Instant,
    capture_origin_x: i32,
    capture_origin_y: i32,
) -> Result<WorkerOutcome> {
    let mut ctx = WorkerContext::new(
        &config,
        layout,
        started_at,
        capture_origin_x,
        capture_origin_y,
    )?;
    let mut stopping = false;
    let mut stop_requested_at = None::<Instant>;

    while !stopping {
        while let Ok(cmd) = control_rx.try_recv() {
            match cmd {
                WorkerCommand::Pause => {
                    let now = Instant::now();
                    ctx.pause(now);
                    capture_stream.pause();
                    if let Some(audio) = audio_stream.as_ref() {
                        audio.pause();
                    }
                }
                WorkerCommand::Resume => {
                    let now = Instant::now();
                    ctx.resume(now);
                    capture_stream.resume();
                    if let Some(audio) = audio_stream.as_ref() {
                        audio.resume();
                    }
                }
                WorkerCommand::Stop => {
                    stop_requested_at = Some(Instant::now());
                    stopping = true;
                    break;
                }
            }
        }

        if stopping {
            break;
        }

        if !ctx.capture_ended {
            match capture_stream.recv_timeout(Duration::from_millis(10)) {
                Ok(event) => ctx.handle_capture_event(event)?,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    ctx.capture_ended = true;
                }
            }
        }

        if let Some(audio) = audio_stream.as_ref() {
            loop {
                match audio.try_recv() {
                    Ok(event) => ctx.handle_audio_event(event)?,
                    Err(snow_audio_recorder::TryRecvError::Empty) => break,
                    Err(snow_audio_recorder::TryRecvError::Closed) => {
                        ctx.audio_ended = true;
                        break;
                    }
                }
            }
        }

        if ctx.capture_ended && (audio_stream.is_none() || ctx.audio_ended) {
            if stop_requested_at.is_none() {
                stop_requested_at = Some(Instant::now());
            }
            break;
        }
    }

    capture_stream.stop();
    for event in capture_stream.stop_and_drain() {
        ctx.handle_capture_event(event)?;
    }

    if let Some(audio) = audio_stream.as_ref() {
        audio.stop();
    }
    if let Some(audio) = audio_stream.take() {
        for event in audio.stop_and_drain() {
            ctx.handle_audio_event(event)?;
        }
        ctx.audio_ended = true;
    }

    let finalize_at = stop_requested_at.unwrap_or_else(Instant::now);
    ctx.finalize(finalize_at)
}

#[cfg(test)]
fn duration_between_timestamps_ms(start_ts: u64, end_ts: u64, fallback_ms: u32) -> u32 {
    let delta = end_ts.saturating_sub(start_ts);
    if delta == 0 {
        return fallback_ms.max(1);
    }

    delta.min(u64::from(u32::MAX)) as u32
}

fn map_cursor_composition_mode(mode: CursorCompositionMode) -> CursorShapeCompositionMode {
    match mode {
        CursorCompositionMode::AlphaBlend => CursorShapeCompositionMode::AlphaBlend,
        CursorCompositionMode::MaskedColor => CursorShapeCompositionMode::MaskedColor,
    }
}

fn audio_packet_to_i16_le_bytes(packet: &snow_audio_recorder::AudioPacket) -> Result<Vec<u8>> {
    match packet.format.sample_format {
        AudioSampleFormat::I16 => {
            if packet.data.len() % 2 != 0 {
                return Err(ScreenRecorderError::Decode(
                    "audio packet i16 payload is not 2-byte aligned".to_string(),
                ));
            }
            Ok(packet.data.clone())
        }
        AudioSampleFormat::F32 => {
            if packet.data.len() % 4 != 0 {
                return Err(ScreenRecorderError::Decode(
                    "audio packet f32 payload is not 4-byte aligned".to_string(),
                ));
            }

            let mut out = Vec::with_capacity(packet.data.len() / 2);
            for chunk in packet.data.chunks_exact(4) {
                let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let quantized = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                out.extend_from_slice(&quantized.to_le_bytes());
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn state_round_trip() {
        for state in [
            RecordingState::Created,
            RecordingState::Running,
            RecordingState::Paused,
            RecordingState::Stopped,
        ] {
            assert_eq!(state, RecordingState::from_u8(state.as_u8()));
        }
    }

    #[test]
    fn duration_between_timestamps_uses_real_delta() {
        assert_eq!(duration_between_timestamps_ms(1_000, 1_133, 42), 133);
    }

    #[test]
    fn duration_between_timestamps_falls_back_when_delta_is_zero() {
        assert_eq!(duration_between_timestamps_ms(2_000, 2_000, 42), 42);
        assert_eq!(duration_between_timestamps_ms(2_000, 1_500, 42), 42);
    }

    #[test]
    fn map_cursor_composition_mode_maps_known_modes() {
        assert_eq!(
            map_cursor_composition_mode(CursorCompositionMode::AlphaBlend),
            CursorShapeCompositionMode::AlphaBlend
        );
        assert_eq!(
            map_cursor_composition_mode(CursorCompositionMode::MaskedColor),
            CursorShapeCompositionMode::MaskedColor
        );
    }

    fn test_worker_context() -> WorkerContext {
        WorkerContext {
            layout: TempLayout {
                output_dir: PathBuf::from("."),
                session_dir: PathBuf::from("."),
                manifest_path: PathBuf::from("manifest.json"),
                video_temp_path: PathBuf::from("video.mp4"),
                audio_system_path: PathBuf::from("audio_system.pcm"),
                audio_mic_path: PathBuf::from("audio_mic.pcm"),
                mouse_path: PathBuf::from("mouse.bin"),
            },
            frame_interval_ms: 16,
            target_fps: 60,
            video_format: RecordingVideoFormat::H264Lossless,
            video_config: VideoEncodeConfig::default(),
            timeline: PauseTimeline::new(Instant::now()),
            capture_origin_x: 10,
            capture_origin_y: 20,
            width: 0,
            height: 0,
            last_encoded_rgba: None,
            last_observed_ts_ms: None,
            mouse_store: MouseStore::new(),
            cursor_shapes_emitted: HashSet::new(),
            last_cursor_frame: None,
            system_audio: None,
            mic_audio: None,
            preview_encoder: None,
            capture_ended: false,
            audio_ended: false,
            recorded_system_audio: false,
            recorded_microphone_audio: false,
        }
    }

    #[test]
    fn recording_accepts_shape_updates_and_references_by_shape_id() {
        let mut ctx = test_worker_context();
        let cursor = snow_capture::CursorData {
            position_x: 100,
            position_y: 200,
            visible: true,
            shape_id: Some(7),
            shape: Some(snow_capture::CursorShape {
                shape_id: 7,
                hotspot_x: 1,
                hotspot_y: 2,
                width: 2,
                height: 2,
                composition_mode: CursorCompositionMode::AlphaBlend,
                shape_rgba: vec![255; 16],
            }),
        };
        ctx.record_cursor_frame(20, &cursor);

        let cursor_same_shape = snow_capture::CursorData {
            position_x: 104,
            position_y: 206,
            visible: true,
            shape_id: Some(7),
            shape: None,
        };
        ctx.record_cursor_frame(40, &cursor_same_shape);

        assert_eq!(ctx.mouse_store.cursor_shapes.len(), 1);
        assert_eq!(ctx.mouse_store.cursor_frames.len(), 2);
        assert_eq!(ctx.mouse_store.cursor_frames[0].shape_id, Some(7));
        assert_eq!(ctx.mouse_store.cursor_frames[0].x, 90);
        assert_eq!(ctx.mouse_store.cursor_frames[0].y, 180);
    }

    #[test]
    fn frame_dropped_generates_synthetic_cursor_frame() {
        let mut ctx = test_worker_context();
        ctx.last_cursor_frame = Some(CursorFrameRecord {
            timestamp_ms: 10,
            x: 12,
            y: 34,
            visible: false,
            shape_id: Some(5),
        });

        ctx.synthesize_cursor_frame_for_drop(26);
        assert_eq!(ctx.mouse_store.cursor_frames.len(), 1);
        assert_eq!(ctx.mouse_store.cursor_frames[0].timestamp_ms, 26);
        assert_eq!(ctx.mouse_store.cursor_frames[0].x, 12);
        assert_eq!(ctx.mouse_store.cursor_frames[0].y, 34);
        assert_eq!(ctx.mouse_store.cursor_frames[0].shape_id, Some(5));
    }
}
