//! Error types: `RecvError`, `TryRecvError`, `RecvTimeoutError`, `ErrorClass`, and `Classify`.

/// Common recv failure modes across all leaf crate stream handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvError {
    /// The stream has closed and no further events will arrive.
    Disconnected,
}

/// Error for non-blocking try_recv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// No events available right now.
    Empty,
    /// The stream has closed.
    Disconnected,
}

/// Error for recv_timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvTimeoutError {
    /// The timeout elapsed before an event arrived.
    Timeout,
    /// The stream has closed.
    Disconnected,
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecvError::Disconnected => write!(f, "stream disconnected"),
        }
    }
}

impl std::error::Error for RecvError {}

impl std::fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryRecvError::Empty => write!(f, "no events available"),
            TryRecvError::Disconnected => write!(f, "stream disconnected"),
        }
    }
}

impl std::error::Error for TryRecvError {}

impl std::fmt::Display for RecvTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecvTimeoutError::Timeout => write!(f, "recv timed out"),
            RecvTimeoutError::Disconnected => write!(f, "stream disconnected"),
        }
    }
}

impl std::error::Error for RecvTimeoutError {}

/// Classification of errors for coordinator decision-making.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Temporary failure; the operation can be retried.
    Transient,
    /// Unrecoverable failure; the stream/session must stop.
    Fatal,
    /// The configuration is invalid; the caller must fix it.
    InvalidConfig,
}

/// Trait for classifying errors. Leaf crates implement this for their error types.
pub trait Classify {
    fn class(&self) -> ErrorClass;
}

// From impls: std::sync::mpsc errors → snow_core error types
//
// These live in snow-core (where the target types are defined) to satisfy
// Rust's orphan rules. Leaf crates that use std::sync::mpsc channels
// (e.g. snow-capture) get these conversions for free.

impl From<std::sync::mpsc::RecvError> for RecvError {
    fn from(_: std::sync::mpsc::RecvError) -> Self {
        RecvError::Disconnected
    }
}

impl From<std::sync::mpsc::TryRecvError> for TryRecvError {
    fn from(e: std::sync::mpsc::TryRecvError) -> Self {
        match e {
            std::sync::mpsc::TryRecvError::Empty => TryRecvError::Empty,
            std::sync::mpsc::TryRecvError::Disconnected => TryRecvError::Disconnected,
        }
    }
}

impl From<std::sync::mpsc::RecvTimeoutError> for RecvTimeoutError {
    fn from(e: std::sync::mpsc::RecvTimeoutError) -> Self {
        match e {
            std::sync::mpsc::RecvTimeoutError::Timeout => RecvTimeoutError::Timeout,
            std::sync::mpsc::RecvTimeoutError::Disconnected => RecvTimeoutError::Disconnected,
        }
    }
}
