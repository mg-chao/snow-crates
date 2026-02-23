use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use snow_audio_recorder::{
    AudioEvent, AudioFormat, AudioSampleFormat, AudioSession, AudioSourceKind, AudioStreamConfig,
    DeviceSelector, SourceConfig,
};
use snow_capture::{CaptureEvent, CaptureMode, CaptureSession, CaptureTarget, StreamConfig};
use uuid::Uuid;

use crate::artifact::{RecordingArtifact, SessionManifest};
use crate::audio::mp3_writer::Mp3FileWriter;
use crate::config::{RecordingAudioFormat, RecordingConfig, RecordingTarget, RecordingVideoFormat};
use crate::error::{Result, ScreenRecorderError};
use crate::mouse::start_mouse_worker;
use crate::temp::TempLayout;
use crate::timeline::PauseTimeline;
use crate::video::encoder_h264_lossless::H264LosslessFileWriter;
use crate::video::frame_cache::{
    FrameBlock, FrameBlockKind, FrameCacheWriter, RectPatch, extract_patch,
};

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
    mouse_stop: Arc<std::sync::atomic::AtomicBool>,
    mouse_handle: JoinHandle<Result<()>>,
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

        if matches!(config.audio.format, RecordingAudioFormat::Aac) {
            return Err(ScreenRecorderError::UnsupportedFeature(
                "AAC is not supported in pure-Rust v1".to_string(),
            ));
        }

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
            .capture_cursor(false)
            .build()?;
        let capture_stream = capture_session.start_streaming(
            capture_target,
            StreamConfig {
                target_fps: self.config.fps,
                buffer_depth: 8,
                max_consecutive_errors: 30,
                adaptive_fps: true,
                min_fps: 10,
                pause_on_resolution_change: false,
            },
        )?;

        let audio_stream = start_audio_stream_if_enabled(&self.config)?;
        let (control_tx, control_rx) = crossbeam_channel::unbounded::<WorkerCommand>();
        let started_at = Instant::now();

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
                )
            })
            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;

        let mouse_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mouse_handle = start_mouse_worker(
            self.layout.mouse_path.clone(),
            Arc::clone(&mouse_stop),
            started_at,
            origin.0,
            origin.1,
        );

        let runtime = RuntimeHandles {
            control_tx,
            worker_handle,
            mouse_stop,
            mouse_handle,
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
        runtime.mouse_stop.store(true, Ordering::Release);

        let worker_result = runtime.worker_handle.join().map_err(|_| {
            ScreenRecorderError::Encode("recording worker thread panicked".to_string())
        })?;
        let mouse_result = runtime
            .mouse_handle
            .join()
            .map_err(|_| ScreenRecorderError::Encode("mouse worker thread panicked".to_string()))?;

        self.state
            .store(RecordingState::Stopped.as_u8(), Ordering::Release);

        mouse_result?;
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

struct WorkerContext {
    frame_interval_ms: u32,
    video_writer: H264LosslessFileWriter,
    frame_cache: FrameCacheWriter,
    system_mp3: Option<Mp3FileWriter>,
    mic_mp3: Option<Mp3FileWriter>,
    timeline: PauseTimeline,
    width: u32,
    height: u32,
    prev_rgba: Option<Vec<u8>>,
    pending_block: Option<FrameBlock>,
    last_keyframe_ts_ms: u64,
    capture_ended: bool,
    audio_ended: bool,
    recorded_system_audio: bool,
    recorded_microphone_audio: bool,
}

impl WorkerContext {
    fn new(config: &RecordingConfig, layout: &TempLayout, started_at: Instant) -> Result<Self> {
        let frame_interval_ms = ((1000.0 / config.fps.max(1) as f32).round() as u32).max(1);

        let system_mp3 = if config.audio.system_audio_enabled {
            Some(Mp3FileWriter::create(
                &layout.audio_system_path,
                config.audio.sample_rate_hz,
                config.audio.channels.channels(),
                config.audio.bitrate_kbps,
            )?)
        } else {
            None
        };

        let mic_mp3 = if config.audio.microphone_enabled {
            Some(Mp3FileWriter::create(
                &layout.audio_mic_path,
                config.audio.sample_rate_hz,
                config.audio.channels.channels(),
                config.audio.bitrate_kbps,
            )?)
        } else {
            None
        };

        Ok(Self {
            frame_interval_ms,
            video_writer: H264LosslessFileWriter::create(&layout.video_temp_path)?,
            frame_cache: FrameCacheWriter::create(&layout.frame_cache_path)?,
            system_mp3,
            mic_mp3,
            timeline: PauseTimeline::new(started_at),
            width: 0,
            height: 0,
            prev_rgba: None,
            pending_block: None,
            last_keyframe_ts_ms: 0,
            capture_ended: false,
            audio_ended: false,
            recorded_system_audio: false,
            recorded_microphone_audio: false,
        })
    }

    fn pause(&mut self, at: Instant) {
        self.timeline.mark_pause(at);
    }

    fn resume(&mut self, at: Instant) {
        self.timeline.mark_resume(at);
    }

    fn handle_capture_event(&mut self, event: CaptureEvent) -> Result<()> {
        match event {
            CaptureEvent::Frame(frame) => self.handle_frame(frame),
            CaptureEvent::FrameDropped { .. } => {
                if let Some(block) = self.pending_block.as_mut() {
                    block.duration_ms = block.duration_ms.saturating_add(self.frame_interval_ms);
                }
                Ok(())
            }
            CaptureEvent::Paused { at } => {
                self.timeline.mark_pause(at);
                Ok(())
            }
            CaptureEvent::Resumed { at, .. } => {
                self.timeline.mark_resume(at);
                Ok(())
            }
            CaptureEvent::ResolutionChanged { .. } => Err(ScreenRecorderError::Encode(
                "resolution changes during recording are not supported in v1".to_string(),
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
        let rgba = frame.as_rgba_bytes();

        if self.width == 0 || self.height == 0 {
            self.width = width;
            self.height = height;
        } else if self.width != width || self.height != height {
            return Err(ScreenRecorderError::Encode(format!(
                "dynamic resolution is unsupported (expected {}x{}, got {}x{})",
                self.width, self.height, width, height
            )));
        }

        let ts = self
            .timeline
            .active_elapsed_ms(frame.metadata.capture_time.unwrap_or_else(Instant::now));

        let is_duplicate = frame.metadata.is_duplicate;
        if is_duplicate {
            if let Some(block) = self.pending_block.as_mut() {
                block.duration_ms = block.duration_ms.saturating_add(self.frame_interval_ms);
            }
            return Ok(());
        }

        self.video_writer.write_rgba_frame(width, height, rgba)?;

        if let Some(block) = self.pending_block.take() {
            self.frame_cache.write_block(&block)?;
        }

        let force_keyframe =
            self.prev_rgba.is_none() || ts.saturating_sub(self.last_keyframe_ts_ms) >= 2000;
        let kind = if !force_keyframe {
            build_delta_kind(&frame, self.width, self.height)?.unwrap_or_else(|| {
                FrameBlockKind::Keyframe {
                    rgba: rgba.to_vec(),
                }
            })
        } else {
            FrameBlockKind::Keyframe {
                rgba: rgba.to_vec(),
            }
        };

        if matches!(kind, FrameBlockKind::Keyframe { .. }) {
            self.last_keyframe_ts_ms = ts;
        }

        self.prev_rgba = Some(rgba.to_vec());
        self.pending_block = Some(FrameBlock {
            timestamp_ms: ts,
            duration_ms: self.frame_interval_ms,
            width: self.width,
            height: self.height,
            kind,
        });
        Ok(())
    }

    fn handle_audio_event(&mut self, event: AudioEvent) -> Result<()> {
        match event {
            AudioEvent::Packet(packet) => {
                let bytes = audio_packet_to_i16_le_bytes(&packet)?;
                match packet.source {
                    AudioSourceKind::System => {
                        if let Some(writer) = self.system_mp3.as_mut() {
                            writer.append_i16_le_bytes(&bytes)?;
                            self.recorded_system_audio =
                                self.recorded_system_audio || !bytes.is_empty();
                        }
                    }
                    AudioSourceKind::Microphone => {
                        if let Some(writer) = self.mic_mp3.as_mut() {
                            writer.append_i16_le_bytes(&bytes)?;
                            self.recorded_microphone_audio =
                                self.recorded_microphone_audio || !bytes.is_empty();
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

        if let Some(block) = self.pending_block.take() {
            self.frame_cache.write_block(&block)?;
        }
        self.frame_cache.flush()?;

        self.video_writer.flush()?;

        if let Some(writer) = self.system_mp3.take() {
            writer.finish()?;
        }
        if let Some(writer) = self.mic_mp3.take() {
            writer.finish()?;
        }

        if self.width == 0 || self.height == 0 {
            return Err(ScreenRecorderError::Encode(
                "recording ended without any video frames".to_string(),
            ));
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
) -> Result<WorkerOutcome> {
    let mut ctx = WorkerContext::new(&config, &layout, started_at)?;
    let mut stopping = false;

    while !stopping {
        while let Ok(cmd) = control_rx.try_recv() {
            match cmd {
                WorkerCommand::Pause => {
                    capture_stream.pause();
                    if let Some(audio) = audio_stream.as_ref() {
                        audio.pause();
                    }
                    ctx.pause(Instant::now());
                }
                WorkerCommand::Resume => {
                    capture_stream.resume();
                    if let Some(audio) = audio_stream.as_ref() {
                        audio.resume();
                    }
                    ctx.resume(Instant::now());
                }
                WorkerCommand::Stop => {
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
            break;
        }
    }

    capture_stream.stop();
    if let Some(audio) = audio_stream.as_ref() {
        audio.stop();
    }

    for event in capture_stream.stop_and_drain() {
        ctx.handle_capture_event(event)?;
    }

    if let Some(audio) = audio_stream.take() {
        for event in audio.stop_and_drain() {
            ctx.handle_audio_event(event)?;
        }
        ctx.audio_ended = true;
    }

    ctx.finalize(Instant::now())
}

fn build_delta_kind(
    frame: &snow_capture::frame::Frame,
    width: u32,
    height: u32,
) -> Result<Option<FrameBlockKind>> {
    let dirty_rects = &frame.metadata.dirty_rects;
    if dirty_rects.is_empty() {
        return Ok(None);
    }

    let frame_area = u64::from(width) * u64::from(height);
    if frame_area == 0 {
        return Ok(None);
    }

    let mut dirty_area: u64 = 0;
    let mut patches = Vec::<RectPatch>::new();
    let rgba = frame.as_rgba_bytes();

    for rect in dirty_rects {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        let safe_w = rect.width.min(width.saturating_sub(rect.x));
        let safe_h = rect.height.min(height.saturating_sub(rect.y));
        if safe_w == 0 || safe_h == 0 {
            continue;
        }

        dirty_area = dirty_area.saturating_add(u64::from(safe_w) * u64::from(safe_h));
        patches.push(extract_patch(
            rgba, width, height, rect.x, rect.y, safe_w, safe_h,
        )?);
    }

    if patches.is_empty() {
        return Ok(None);
    }

    if (dirty_area as f64 / frame_area as f64) > 0.40 {
        return Ok(None);
    }

    Ok(Some(FrameBlockKind::Delta { patches }))
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
    fn create_rejects_aac() {
        let mut config = RecordingConfig::default();
        config.audio.format = RecordingAudioFormat::Aac;
        config.output_dir = std::env::temp_dir().join(format!(
            "snow-screen-recorder-test-{}",
            Uuid::new_v4().simple()
        ));

        match RecordingSession::create(config) {
            Ok(_) => panic!("AAC should be rejected in v1"),
            Err(err) => assert!(matches!(err, ScreenRecorderError::UnsupportedFeature(_))),
        }
    }
}
