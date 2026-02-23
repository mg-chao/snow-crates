use std::fmt;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum AudioError {
    InvalidConfig(String),
    DeviceUnavailable(String),
    DeviceLost,
    AccessDenied,
    UnsupportedFormat(String),
    BufferOverflow,
    WorkerDead,
    Canceled,
    BackendUnavailable(String),
    Platform(Arc<anyhow::Error>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioErrorClass {
    InvalidInput,
    Unsupported,
    Transient,
    Fatal,
}

impl AudioError {
    /// Wrap an `anyhow::Error` (or anything convertible to one) in the
    /// `Platform` variant. The inner error is stored behind an `Arc` so
    /// that `AudioError` remains `Clone`.
    pub fn platform(err: impl Into<anyhow::Error>) -> Self {
        Self::Platform(Arc::new(err.into()))
    }

    pub fn class(&self) -> AudioErrorClass {
        match self {
            Self::InvalidConfig(_) | Self::DeviceUnavailable(_) => AudioErrorClass::InvalidInput,
            Self::UnsupportedFormat(_) | Self::BackendUnavailable(_) => AudioErrorClass::Unsupported,
            Self::DeviceLost | Self::Canceled | Self::WorkerDead => AudioErrorClass::Transient,
            Self::AccessDenied | Self::BufferOverflow | Self::Platform(_) => AudioErrorClass::Fatal,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self.class(), AudioErrorClass::Transient)
    }

    pub fn requires_worker_reset(&self) -> bool {
        matches!(self, Self::DeviceLost | Self::WorkerDead)
    }
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid audio configuration: {msg}"),
            Self::DeviceUnavailable(msg) => write!(f, "audio device unavailable: {msg}"),
            Self::DeviceLost => write!(f, "audio device was invalidated or disconnected"),
            Self::AccessDenied => write!(f, "audio device access denied"),
            Self::UnsupportedFormat(msg) => write!(f, "unsupported audio format: {msg}"),
            Self::BufferOverflow => write!(f, "audio buffer overflow"),
            Self::WorkerDead => write!(f, "audio worker is not running"),
            Self::Canceled => write!(f, "audio operation canceled"),
            Self::BackendUnavailable(msg) => write!(f, "audio backend unavailable: {msg}"),
            Self::Platform(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Platform(inner) => Some(inner.as_ref().as_ref()),
            _ => None,
        }
    }
}

pub type AudioResult<T> = Result<T, AudioError>;

/// Error returned by [`AudioStreamHandle::recv`] when the stream has closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "audio stream closed")
    }
}

impl std::error::Error for RecvError {}

/// Error returned by [`AudioStreamHandle::try_recv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// No events are available right now.
    Empty,
    /// The stream has closed and no further events will arrive.
    Closed,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "no audio event available"),
            Self::Closed => write!(f, "audio stream closed"),
        }
    }
}

impl std::error::Error for TryRecvError {}

/// Error returned by [`AudioStreamHandle::recv_timeout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvTimeoutError {
    /// The timeout elapsed before an event arrived.
    Timeout,
    /// The stream has closed and no further events will arrive.
    Closed,
}

impl fmt::Display for RecvTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "audio recv timed out"),
            Self::Closed => write!(f, "audio stream closed"),
        }
    }
}

impl std::error::Error for RecvTimeoutError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_and_reset_semantics_are_stable() {
        assert!(AudioError::DeviceLost.is_retryable());
        assert!(AudioError::WorkerDead.requires_worker_reset());
        assert!(!AudioError::InvalidConfig("x".into()).is_retryable());
        assert!(!AudioError::AccessDenied.requires_worker_reset());
    }

    #[test]
    fn platform_error_clone_preserves_message() {
        let err = AudioError::platform(anyhow::anyhow!("root cause"));
        let cloned = err.clone();
        let rendered = cloned.to_string();
        assert!(rendered.contains("root cause"));
    }
}
