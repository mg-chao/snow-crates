use std::time::{Duration, Instant};

use snow_audio_recorder::{AudioFormat, AudioSourceKind};
use snow_cursor_capture::CursorFrameSample;

/// Unified event type for the recording coordinator.
/// All subsystem events are normalized into this enum so the
/// event loop can use `crossbeam::select!` across per-adapter channels.
pub(crate) enum RecordingEvent {
    Video(VideoCaptureEvent),
    Audio(AudioCaptureEvent),
    Cursor(CursorCaptureEvent),
}

#[derive(Debug)]
pub(crate) enum VideoCaptureEvent {
    Frame {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        timestamp: StreamTimestamp,
        is_duplicate: bool,
    },
    ResolutionChanged {
        old_width: u32,
        old_height: u32,
        new_width: u32,
        new_height: u32,
    },
    FrameDropped {
        sequence: u64,
    },
    /// The capture backend paused. Carries the backend's own timestamp
    /// for accurate pause-point tracking.
    Paused {
        at: Instant,
    },
    /// The capture backend resumed. Carries the backend's own timestamp
    /// and the measured gap duration.
    Resumed {
        at: Instant,
        gap: Duration,
    },
    StreamEnded,
    Error(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug)]
pub(crate) enum AudioCaptureEvent {
    Packet {
        source: AudioSourceKind,
        data: Vec<u8>,
        frames: u32,
        format: AudioFormat,
        timestamp: StreamTimestamp,
    },
    PacketDropped {
        source: AudioSourceKind,
        dropped_frames: u64,
    },
    SourceRestarted {
        source: AudioSourceKind,
        old_device_id: Option<String>,
        new_device_id: String,
        downtime: Duration,
    },
    Paused {
        at: Instant,
    },
    Resumed {
        at: Instant,
        gap: Duration,
    },
    BufferPressure {
        fill_ratio: f64,
        buffer_depth: usize,
    },
    StreamEnded,
    Error(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug)]
pub(crate) enum CursorCaptureEvent {
    Sample(CursorFrameSample),
    StreamEnded,
    Error(Box<dyn std::error::Error + Send + Sync>),
}

/// Normalized timestamp that works across subsystems.
/// Both snow-capture (`present_time_qpc`) and snow-audio-recorder
/// (`qpc_position_100ns`) provide a 100ns-resolution QPC-derived value
/// on Windows, so we carry that plus `Instant` for fallback.
#[derive(Clone, Debug)]
pub(crate) struct StreamTimestamp {
    pub instant: Instant,
    pub qpc_100ns: Option<i64>,
}

pub(crate) enum ControlCommand {
    Pause,
    Resume,
    Stop,
}

pub(crate) enum EventAction {
    Continue,
    Stop,
}

impl EventAction {
    pub(crate) fn is_stop(&self) -> bool {
        matches!(self, EventAction::Stop)
    }
}


/// Coordinator stop reason evaluated in precedence order.
/// Higher variants take priority over lower ones.
pub(crate) enum TerminationCondition {
    FatalVideoError,
    ControlStop,
    AllStreamsEnded,
    AllChannelsDisconnected,
}
