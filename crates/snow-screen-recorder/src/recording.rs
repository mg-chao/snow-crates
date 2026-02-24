use std::collections::HashMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use snow_audio_recorder::{
    AudioEvent, AudioFormat, AudioPacket, AudioSampleFormat, AudioSession, AudioSourceKind,
    AudioStreamConfig, AudioTimestampAnchor, DeviceSelector, SourceConfig, align_packet_frames,
};
use snow_capture::{CaptureEvent, CaptureMode, CaptureSession, CaptureTarget, StreamConfig};
use uuid::Uuid;

use crate::artifact::{RecordingArtifact, SessionManifest};
use crate::config::{RecordingConfig, RecordingTarget, RecordingVideoFormat};
use crate::error::{Result, ScreenRecorderError};
use crate::model::{StoredFrame, write_frames};
use crate::mouse::{CursorSampleRecord, CursorShapeRecord, MouseRecord, write_mouse_records};
use crate::temp::TempLayout;
use crate::timeline::PauseTimeline;

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

        if !matches!(config.video_format, RecordingVideoFormat::H264Lossless) {
            return Err(ScreenRecorderError::UnsupportedFeature(
                "only H264Lossless is supported in v1".to_string(),
            ));
        }

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
            video_temp_path: self.layout.video_temp_path.clone(),
            frame_cache_path: self.layout.frame_cache_path.clone(),
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

struct WorkerContext {
    layout: TempLayout,
    frame_interval_ms: u32,
    timeline: PauseTimeline,
    capture_origin_x: i32,
    capture_origin_y: i32,
    width: u32,
    height: u32,
    frames: Vec<StoredFrame>,
    pending_frame: Option<StoredFrame>,
    last_observed_ts_ms: Option<u64>,
    mouse_records: Vec<MouseRecord>,
    cursor_shape_ids: HashMap<u64, u32>,
    next_cursor_shape_id: u32,
    active_cursor_shape_id: Option<u32>,
    system_audio: Option<PcmTrackWriter>,
    mic_audio: Option<PcmTrackWriter>,
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
            timeline: PauseTimeline::new(started_at),
            capture_origin_x,
            capture_origin_y,
            width: 0,
            height: 0,
            frames: Vec::new(),
            pending_frame: None,
            last_observed_ts_ms: None,
            mouse_records: Vec::new(),
            cursor_shape_ids: HashMap::new(),
            next_cursor_shape_id: 1,
            active_cursor_shape_id: None,
            system_audio,
            mic_audio,
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
        if let Some(frame) = self.pending_frame.as_mut() {
            frame.duration_ms = duration_between_timestamps_ms(
                frame.timestamp_ms,
                ts_ms,
                self.frame_interval_ms.max(1),
            );
        }
    }

    fn remember_cursor_shape(&mut self, cursor: &snow_capture::CursorData) -> Option<u32> {
        let Some((shape_hash, shape_bytes_len)) = cursor_shape_hash(cursor) else {
            return self.active_cursor_shape_id;
        };

        let shape_id = if let Some(existing) = self.cursor_shape_ids.get(&shape_hash).copied() {
            existing
        } else {
            let shape_id = self.next_cursor_shape_id;
            if let Some(next_id) = self.next_cursor_shape_id.checked_add(1) {
                self.next_cursor_shape_id = next_id;
            }
            self.cursor_shape_ids.insert(shape_hash, shape_id);
            self.mouse_records
                .push(MouseRecord::CursorShape(CursorShapeRecord {
                    shape_id,
                    shape_hash,
                    hotspot_x: cursor.hotspot_x,
                    hotspot_y: cursor.hotspot_y,
                    width: cursor.shape_width,
                    height: cursor.shape_height,
                    shape_rgba: cursor.shape_rgba[..shape_bytes_len].to_vec(),
                }));
            shape_id
        };

        self.active_cursor_shape_id = Some(shape_id);
        Some(shape_id)
    }

    fn handle_capture_event(&mut self, event: CaptureEvent) -> Result<()> {
        match event {
            CaptureEvent::Frame(frame) => self.handle_frame(frame),
            CaptureEvent::FrameDropped { .. } => {
                if let Some(last) = self.last_observed_ts_ms {
                    self.observe_video_time(last.saturating_add(u64::from(self.frame_interval_ms)));
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
            let shape_id = self.remember_cursor_shape(cursor);
            self.mouse_records
                .push(MouseRecord::CursorSample(CursorSampleRecord {
                    timestamp_ms: ts_ms,
                    x: cursor.position_x - self.capture_origin_x,
                    y: cursor.position_y - self.capture_origin_y,
                    visible: cursor.visible,
                    shape_id,
                }));
        }

        if frame.metadata.is_duplicate {
            return Ok(());
        }

        if let Some(done) = self.pending_frame.take() {
            self.frames.push(done);
        }

        self.pending_frame = Some(StoredFrame {
            timestamp_ms: ts_ms,
            duration_ms: self.frame_interval_ms,
            width,
            height,
            rgba: frame.as_rgba_bytes().to_vec(),
        });
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

        if let Some(done) = self.pending_frame.take() {
            self.frames.push(done);
        }

        if self.frames.is_empty() {
            return Err(ScreenRecorderError::Encode(
                "recording ended without any video frames".to_string(),
            ));
        }

        write_frames(&self.layout.frame_cache_path, &self.frames)?;
        write_mouse_records(&self.layout.mouse_path, &self.mouse_records)?;

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

fn duration_between_timestamps_ms(start_ts: u64, end_ts: u64, fallback_ms: u32) -> u32 {
    let delta = end_ts.saturating_sub(start_ts);
    if delta == 0 {
        return fallback_ms.max(1);
    }

    delta.min(u64::from(u32::MAX)) as u32
}

fn cursor_shape_hash(cursor: &snow_capture::CursorData) -> Option<(u64, usize)> {
    if cursor.shape_width == 0 || cursor.shape_height == 0 {
        return None;
    }
    let width = usize::try_from(cursor.shape_width).ok()?;
    let height = usize::try_from(cursor.shape_height).ok()?;
    let shape_bytes_len = width.checked_mul(height)?.checked_mul(4)?;
    if cursor.shape_rgba.len() < shape_bytes_len {
        return None;
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cursor.hotspot_x.hash(&mut hasher);
    cursor.hotspot_y.hash(&mut hasher);
    cursor.shape_width.hash(&mut hasher);
    cursor.shape_height.hash(&mut hasher);
    cursor.shape_rgba[..shape_bytes_len].hash(&mut hasher);
    Some((hasher.finish(), shape_bytes_len))
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
    fn cursor_shape_hash_requires_valid_rgba_payload() {
        let cursor = snow_capture::CursorData {
            hotspot_x: 1,
            hotspot_y: 2,
            position_x: 100,
            position_y: 200,
            visible: true,
            shape_width: 8,
            shape_height: 8,
            shape_rgba: vec![0; 8 * 8 * 4 - 1],
        };
        assert!(cursor_shape_hash(&cursor).is_none());
    }

    #[test]
    fn cursor_shape_hash_changes_when_shape_changes() {
        let mut cursor = snow_capture::CursorData {
            hotspot_x: 1,
            hotspot_y: 2,
            position_x: 100,
            position_y: 200,
            visible: true,
            shape_width: 2,
            shape_height: 2,
            shape_rgba: vec![0, 0, 0, 0, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        };

        let first = cursor_shape_hash(&cursor)
            .expect("shape hash should exist")
            .0;
        cursor.shape_rgba[5] ^= 0xFF;
        let second = cursor_shape_hash(&cursor)
            .expect("shape hash should exist")
            .0;
        assert_ne!(first, second);
    }
}
