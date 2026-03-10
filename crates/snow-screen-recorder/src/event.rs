use snow_audio_recorder::AudioEvent;
use snow_capture::CaptureEvent;
use snow_core::event::TaggedEvent;
use snow_cursor_capture::CursorEvent;

/// Thin transport enum for the recording coordinator.
///
/// Each variant carries the original leaf event type wrapped in a
/// [`TaggedEvent`] — no field-by-field payload mirroring. Adding fields
/// to a leaf event requires zero changes here.
pub(crate) enum RecordingEvent {
    Video(TaggedEvent<CaptureEvent>),
    Audio(TaggedEvent<AudioEvent>),
    Cursor(TaggedEvent<CursorEvent>),
}

pub(crate) enum ControlCommand {
    Pause,
    Resume,
    Stop,
}

pub(crate) enum EventAction {
    Continue,
    Stop,
}

impl EventAction {
    pub(crate) fn is_stop(&self) -> bool {
        matches!(self, EventAction::Stop)
    }
}
