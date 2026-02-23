use std::time::{Duration, Instant};

use crate::error::AudioError;
use crate::format::AudioFormat;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceKind {
    System,
    Microphone,
}

#[derive(Clone, Debug, Default)]
pub struct AudioPacketMetadata {
    pub capture_time: Option<Instant>,
    pub qpc_position_100ns: Option<i64>,
    pub device_position_frames: Option<u64>,
    pub discontinuity: bool,
    pub is_silent: bool,
    pub sequence: u64,
}

#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub source: AudioSourceKind,
    pub format: AudioFormat,
    pub frames: u32,
    pub data: Vec<u8>,
    pub metadata: AudioPacketMetadata,
}

#[derive(Clone, Debug)]
pub enum AudioEvent {
    Packet(AudioPacket),
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
    StreamEnded,
    Error(AudioError),
}
