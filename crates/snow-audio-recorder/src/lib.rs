pub mod backend;
pub mod device;
pub mod error;
pub(crate) mod event_queue;
pub mod format;
pub mod packet;
mod platform;
pub mod session;
pub mod streaming;
pub mod timeline;

pub use backend::AudioBackendKind;
pub use device::{AudioDeviceInfo, DeviceFlow, DeviceSelector};
pub use error::{
    AudioError, AudioErrorClass, AudioResult, RecvError, RecvTimeoutError, TryRecvError,
};
pub use format::{AudioFormat, AudioSampleFormat, MAX_CHANNELS};
pub use packet::{AudioEvent, AudioPacket, AudioPacketMetadata, AudioSourceKind};
pub use session::{
    AudioSession, AudioSessionBuilder, AudioStreamConfig, RestartPolicy, SourceConfig,
};
pub use streaming::{AudioStreamHandle, AudioStreamStats, AudioStreamStatsSnapshot};
pub use timeline::{
    AudioPacketAlignment, AudioPacketTimestamp, AudioTimestampAnchor,
    align_i16_interleaved_to_duration, align_packet_frames, duration_to_frames_round,
};
