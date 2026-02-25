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

impl snow_core::error::Classify for CursorCaptureError {
    fn class(&self) -> snow_core::error::ErrorClass {
        match self {
            Self::UnsupportedPlatform => snow_core::error::ErrorClass::InvalidConfig,
            // Cannot reliably distinguish transient vs fatal from a string message;
            // default to Transient for parity with current recorder behavior.
            Self::Platform(_) => snow_core::error::ErrorClass::Transient,
        }
    }
}

