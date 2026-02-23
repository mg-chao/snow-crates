use std::fmt;

#[derive(Debug)]
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
    Platform(anyhow::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioErrorClass {
    InvalidInput,
    Unsupported,
    Transient,
    Fatal,
}

impl AudioError {
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

    pub fn to_sendable(&self) -> Self {
        match self {
            Self::InvalidConfig(v) => Self::InvalidConfig(v.clone()),
            Self::DeviceUnavailable(v) => Self::DeviceUnavailable(v.clone()),
            Self::DeviceLost => Self::DeviceLost,
            Self::AccessDenied => Self::AccessDenied,
            Self::UnsupportedFormat(v) => Self::UnsupportedFormat(v.clone()),
            Self::BufferOverflow => Self::BufferOverflow,
            Self::WorkerDead => Self::WorkerDead,
            Self::Canceled => Self::Canceled,
            Self::BackendUnavailable(v) => Self::BackendUnavailable(v.clone()),
            Self::Platform(err) => Self::Platform(anyhow::anyhow!("{err:#}")),
        }
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
            Self::Platform(inner) => Some(inner.as_ref()),
            _ => None,
        }
    }
}

pub type AudioResult<T> = Result<T, AudioError>;

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
    fn sendable_platform_error_flattens_chain() {
        let err = AudioError::Platform(anyhow::anyhow!("root cause"));
        let sendable = err.to_sendable();
        let rendered = sendable.to_string();
        assert!(rendered.contains("root cause"));
    }
}
