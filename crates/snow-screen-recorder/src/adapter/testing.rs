//! Mock adapter framework for deterministic testing.
//!
//! Provides `MockAdapter` — a `StreamAdapter` implementation that writes
//! scripted event sequences directly to crossbeam channels, enabling
//! deterministic testing of the event loop and coordinator without real
//! capture backends.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::Sender;

use crate::adapter::StreamAdapter;
use crate::error::Result;
use crate::event::RecordingEvent;

/// A mock `StreamAdapter` that feeds scripted events into a crossbeam channel.
///
/// Events are sent eagerly on construction (via `send_all`) or lazily
/// via `send_remaining`. The adapter tracks running state and supports
/// the full `StreamAdapter` lifecycle (pause/resume/stop/join).
pub(crate) struct MockAdapter {
    running: Arc<AtomicBool>,
    events: Vec<RecordingEvent>,
    tx: Sender<RecordingEvent>,
    sent: bool,
}

impl MockAdapter {
    /// Create a new `MockAdapter` with a scripted event sequence.
    ///
    /// Events are NOT sent until `send_all()` is called, allowing the
    /// test to control timing.
    pub(crate) fn new(events: Vec<RecordingEvent>, tx: Sender<RecordingEvent>) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            events,
            tx,
            sent: false,
        }
    }

    /// Send all scripted events into the channel immediately.
    /// Returns the number of events successfully sent.
    pub(crate) fn send_all(&mut self) -> usize {
        if self.sent {
            return 0;
        }
        self.sent = true;
        let events = std::mem::take(&mut self.events);
        let mut count = 0;
        for event in events {
            if self.tx.send(event).is_ok() {
                count += 1;
            } else {
                break;
            }
        }
        count
    }
}

impl StreamAdapter for MockAdapter {
    fn pause(&self) -> Result<()> {
        Ok(())
    }

    fn resume(&self) -> Result<()> {
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn join(&mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Builder for constructing `RecordingAdapters` with mock adapters
/// for deterministic testing.
///
/// Provides a fluent API for scripting events on each channel and
/// building the complete adapter set.
pub(crate) struct MockAdapterBuilder {
    video_events: Vec<RecordingEvent>,
    audio_events: Vec<RecordingEvent>,
    cursor_events: Vec<RecordingEvent>,
}

impl MockAdapterBuilder {
    pub(crate) fn new() -> Self {
        Self {
            video_events: Vec::new(),
            audio_events: Vec::new(),
            cursor_events: Vec::new(),
        }
    }

    /// Add events to the video channel script.
    pub(crate) fn video_events(mut self, events: Vec<RecordingEvent>) -> Self {
        self.video_events = events;
        self
    }

    /// Add events to the audio channel script.
    pub(crate) fn audio_events(mut self, events: Vec<RecordingEvent>) -> Self {
        self.audio_events = events;
        self
    }

    /// Add events to the cursor channel script.
    pub(crate) fn cursor_events(mut self, events: Vec<RecordingEvent>) -> Self {
        self.cursor_events = events;
        self
    }

    /// Build the mock adapters and channels.
    ///
    /// Returns `(RecordingAdapters, control_tx)` where `control_tx` can
    /// be used to send `ControlCommand`s during the test.
    pub(crate) fn build(
        self,
    ) -> (
        crate::adapter::RecordingAdapters,
        crossbeam_channel::Sender<crate::event::ControlCommand>,
    ) {
        let (video_tx, video_rx) = crossbeam_channel::bounded(64);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(64);
        let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(64);
        let (control_tx, control_rx) = crossbeam_channel::unbounded();

        let mut video_adapter = MockAdapter::new(self.video_events, video_tx);
        let mut audio_adapter = MockAdapter::new(self.audio_events, audio_tx);
        let mut cursor_adapter = MockAdapter::new(self.cursor_events, cursor_tx);

        // Send all scripted events immediately.
        video_adapter.send_all();
        audio_adapter.send_all();
        cursor_adapter.send_all();

        let adapters = crate::adapter::RecordingAdapters {
            video_rx,
            audio_rx,
            cursor_rx,
            control_rx,
            video_adapter: Box::new(video_adapter),
            audio_adapter: Some(Box::new(audio_adapter)),
            cursor_adapter: Some(Box::new(cursor_adapter)),
        };

        (adapters, control_tx)
    }
}
