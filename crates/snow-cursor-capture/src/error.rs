use std::fmt;

#[derive(Debug)]
pub enum CursorCaptureError {
    UnsupportedPlatform,
    Platform(String),
}

impl CursorCaptureError {
    pub fn platform(message: impl Into<String>) -> Self {
        Self::Platform(message.into())
    }
}

impl fmt::Display for CursorCaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "cursor capture is only supported on Windows"),
            Self::Platform(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CursorCaptureError {}
