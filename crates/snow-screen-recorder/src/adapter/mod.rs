pub(crate) mod audio;
#[cfg(not(feature = "cursor"))]
pub(crate) mod cursor;
pub(crate) mod multiplexer_setup;
pub(crate) mod stream_bridge;
pub(crate) mod video;

use crate::error::Result;
use crate::event::{ControlCommand, RecordingEvent};

/// Channel capacity for video events.
pub(crate) const VIDEO_CHANNEL_CAPACITY: usize = 8;
/// Channel capacity for audio events.
pub(crate) const AUDIO_CHANNEL_CAPACITY: usize = 4;
/// Channel capacity for cursor events.
pub(crate) const CURSOR_CHANNEL_CAPACITY: usize = 4;

/// Commands sent to adapter forwarding threads via a command channel.
///
/// The adapter object stores a `cmd_tx` sender; `pause()`/`resume()`/`stop()`
/// enqueue commands. The forwarding thread receives them on `cmd_rx` and
/// applies them to the owned leaf-crate handle.
pub(crate) enum AdapterCommand {
    Pause,
    Resume,
    Stop,
}

/// Trait for subsystem stream adapters.
///
/// Each adapter owns a forwarding thread that reads from the leaf
/// crate's native channel and writes to a crossbeam channel. The
/// trait provides lifecycle control and thread joining.
pub(crate) trait StreamAdapter: Send {
    /// Signal the underlying stream to pause.
    fn pause(&self) -> Result<()>;
    /// Signal the underlying stream to resume.
    fn resume(&self) -> Result<()>;
    /// Signal the underlying stream to stop. Does not block.
    fn stop(&self) -> Result<()>;
    /// Returns false once the forwarding thread has fully exited.
    fn is_running(&self) -> bool;
    /// Join the forwarding thread, blocking until it exits.
    /// After join returns, no more events will be sent to the
    /// adapter's crossbeam channel. Remaining events live in
    /// the channel and must be drained by the coordinator.
    /// Calling join before start must return Ok(()) without blocking.
    fn join(&mut self) -> Result<()>;
}

/// Sender halves of the per-adapter crossbeam channels.
///
/// Passed to adapter `start()` methods so each forwarding thread
/// can send events into its dedicated channel.
pub(crate) struct AdapterSenders {
    pub video_tx: crossbeam_channel::Sender<RecordingEvent>,
    pub audio_tx: crossbeam_channel::Sender<RecordingEvent>,
    pub cursor_tx: crossbeam_channel::Sender<RecordingEvent>,
}

/// Receiver halves of the per-adapter crossbeam channels.
///
/// Held by the coordinator for `crossbeam::select!` multiplexing.
pub(crate) struct AdapterReceivers {
    pub video_rx: crossbeam_channel::Receiver<RecordingEvent>,
    pub audio_rx: crossbeam_channel::Receiver<RecordingEvent>,
    pub cursor_rx: crossbeam_channel::Receiver<RecordingEvent>,
}

/// Create bounded crossbeam channel pairs for all data subsystems.
///
/// Each channel has an independent capacity so subsystems apply
/// independent backpressure (video can tolerate frame drops while
/// audio should never drop packets).
pub(crate) fn create_adapter_channels() -> (AdapterSenders, AdapterReceivers) {
    let (video_tx, video_rx) = crossbeam_channel::bounded(VIDEO_CHANNEL_CAPACITY);
    let (audio_tx, audio_rx) = crossbeam_channel::bounded(AUDIO_CHANNEL_CAPACITY);
    let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(CURSOR_CHANNEL_CAPACITY);
    (
        AdapterSenders {
            video_tx,
            audio_tx,
            cursor_tx,
        },
        AdapterReceivers {
            video_rx,
            audio_rx,
            cursor_rx,
        },
    )
}

/// Holds per-adapter channel receivers and adapter objects for the
/// recording worker event loop.
///
/// The coordinator uses `crossbeam::select!` across the data receivers
/// and the control receiver. Adapter objects are retained for lifecycle
/// control (stop/join) during shutdown.
pub(crate) struct RecordingAdapters {
    pub video_rx: crossbeam_channel::Receiver<RecordingEvent>,
    pub audio_rx: crossbeam_channel::Receiver<RecordingEvent>,
    pub cursor_rx: crossbeam_channel::Receiver<RecordingEvent>,
    pub control_rx: crossbeam_channel::Receiver<ControlCommand>,
    pub video_adapter: Box<dyn StreamAdapter>,
    pub audio_adapter: Option<Box<dyn StreamAdapter>>,
    pub cursor_adapter: Option<Box<dyn StreamAdapter>>,
}

impl RecordingAdapters {
    /// Signal all active adapters to pause. Non-blocking.
    pub(crate) fn pause_all(&self) {
        let _ = self.video_adapter.pause();
        if let Some(ref a) = self.audio_adapter {
            let _ = a.pause();
        }
        if let Some(ref a) = self.cursor_adapter {
            let _ = a.pause();
        }
    }

    /// Signal all active adapters to resume. Non-blocking.
    pub(crate) fn resume_all(&self) {
        let _ = self.video_adapter.resume();
        if let Some(ref a) = self.audio_adapter {
            let _ = a.resume();
        }
        if let Some(ref a) = self.cursor_adapter {
            let _ = a.resume();
        }
    }

    /// Signal all active adapters to stop. Non-blocking.
    pub(crate) fn stop_all(&self) {
        let _ = self.video_adapter.stop();
        if let Some(ref a) = self.audio_adapter {
            let _ = a.stop();
        }
        if let Some(ref a) = self.cursor_adapter {
            let _ = a.stop();
        }
    }

    /// Join all active adapter forwarding threads, blocking until each exits.
    pub(crate) fn join_all(&mut self) {
        let _ = self.video_adapter.join();
        if let Some(ref mut a) = self.audio_adapter {
            let _ = a.join();
        }
        if let Some(ref mut a) = self.cursor_adapter {
            let _ = a.join();
        }
    }

    /// Returns `true` if any adapter forwarding thread is still running.
    pub(crate) fn any_running(&self) -> bool {
        self.video_adapter.is_running()
            || self.audio_adapter.as_ref().is_some_and(|a| a.is_running())
            || self.cursor_adapter.as_ref().is_some_and(|a| a.is_running())
    }
}
