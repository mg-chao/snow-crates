pub mod backend;
pub mod device;
pub mod error;
pub(crate) mod event_queue;
pub mod format;
pub mod packet;
pub mod session;
pub mod streaming;
mod platform;

pub use backend::AudioBackendKind;
pub use device::{AudioDeviceInfo, DeviceFlow, DeviceSelector};
pub use error::{AudioError, AudioErrorClass, AudioResult};
pub use format::{AudioFormat, AudioSampleFormat};
pub use packet::{AudioEvent, AudioPacket, AudioPacketMetadata, AudioSourceKind};
pub use session::{AudioSession, AudioSessionBuilder, AudioStreamConfig, RestartPolicy, SourceConfig};
pub use streaming::{AudioStreamHandle, AudioStreamStats, AudioStreamStatsSnapshot};
