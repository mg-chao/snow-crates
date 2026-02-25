use std::fmt;

use snow_core::error::{Classify, ErrorClass};

#[derive(Debug)]
pub enum CursorCaptureError {
    /// The current platform does not support cursor capture.
    UnsupportedPlatform,
    /// A platform-specific error with a descriptive message.
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

impl Classify for CursorCaptureError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::UnsupportedPlatform => ErrorClass::InvalidConfig,
            // Cannot reliably distinguish transient vs fatal from a string message;
            // default to Transient for parity with current recorder behavior.
            Self::Platform(_) => ErrorClass::Transient,
        }
    }
}
