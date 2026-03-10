pub mod artifact;
pub mod config;
pub mod editing;
pub mod error;
pub mod export;
pub mod mouse;
pub mod recording;
pub mod temp;

pub(crate) mod adapter;
pub(crate) mod coordinator;
pub(crate) mod event;
pub(crate) mod event_loop;
pub(crate) mod ffmpeg_util;
pub(crate) mod model;
pub(crate) mod processor;
pub(crate) mod timeline;
pub(crate) mod video_quality;

pub use artifact::RecordingArtifact;
pub use config::{
    AudioChannels, AudioEditConfig, ExportFormat, ExportPerformanceConfig, ExportPreset,
    ExportRequest, HardwarePolicy, MonitorSelector, MouseEditConfig, RecordingAudioConfig,
    RecordingAudioFormat, RecordingConfig, RecordingRegion, RecordingTarget, RecordingVideoFormat,
    VideoEncodeConfig, VideoEncodingSpeed, WindowSelector,
};
pub use editing::EditingSession;
pub use error::ScreenRecorderError;
pub use export::{ExportProgress, ExportResult, ExportStage, ExportTask};
pub use recording::{RecordingSession, RecordingState};
