use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScreenRecorderError {
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),

    #[error("capture error: {0}")]
    Capture(#[from] snow_capture::error::CaptureError),

    #[error("audio error: {0}")]
    Audio(#[from] snow_audio_recorder::error::AudioError),

    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("encode error: {0}")]
    Encode(String),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("export error: {0}")]
    Export(String),
}

pub type Result<T> = std::result::Result<T, ScreenRecorderError>;
