pub mod artifact;
pub mod config;
pub mod editing;
pub mod error;
pub mod export;
pub mod mouse;
pub mod recording;
pub mod temp;

pub(crate) mod model;
pub(crate) mod timeline;
pub(crate) mod video_quality;

pub use artifact::RecordingArtifact;
pub use config::{
    AudioChannels, AudioEditConfig, EditConfig, ExportConfig, ExportFormat, MouseEditConfig,
    RecordingAudioConfig, RecordingAudioFormat, RecordingConfig, RecordingTarget,
    RecordingVideoFormat, VideoEncodeConfig, VideoEncodingSpeed,
};
pub use editing::EditingSession;
pub use error::ScreenRecorderError;
pub use export::ExportResult;
pub use recording::{RecordingSession, RecordingState};
