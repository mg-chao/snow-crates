//! Builds a `StreamMultiplexer<RecordingEvent>` from leaf crate stream handles.
//!
//! Replaces the per-source `StreamBridge` + crossbeam channel wiring with a
//! single multiplexer that handles forwarding threads, per-source channels,
//! audio-priority drain, and select-based multiplexing internally.

use std::time::Duration;

use smallvec::{smallvec, SmallVec};
use snow_audio_recorder::AudioStreamHandle;
use snow_capture::CaptureEvent;
use snow_core::event::{SourceId, TaggedEvent};
use snow_core::multiplexer::{
    MultiplexerConfig, SourceConfig, StreamMultiplexer, StreamMultiplexerBuilder,
};
use snow_core::timestamp::{StreamTimestamp, TickFormat};
use snow_cursor_capture::{CursorCaptureError, CursorEvent};

use crate::coordinator::{AUDIO_SOURCE, CURSOR_SOURCE, VIDEO_SOURCE};
use crate::event::RecordingEvent;

/// Default send timeout for per-source channels (10ms).
const SOURCE_SEND_TIMEOUT: Duration = Duration::from_millis(10);

/// Channel capacity for video source.
const VIDEO_CHANNEL_CAPACITY: usize = 8;
/// Channel capacity for audio source.
const AUDIO_CHANNEL_CAPACITY: usize = 4;
/// Channel capacity for cursor source (standalone path).
const CURSOR_CHANNEL_CAPACITY: usize = 4;

/// Build a `StreamMultiplexer<RecordingEvent>` from the given leaf stream handles.
///
/// Registers:
/// - Video source with one-to-many mapper (extracts cursor when `cursor` feature enabled)
/// - Audio source (simple 1:1 mapper) if `audio_handle` is `Some`
/// - Standalone cursor source (when `cursor` feature disabled and handle provided)
///
/// Returns the built multiplexer and the set of registered source IDs.
pub(crate) fn build_multiplexer(
    video_handle: snow_capture::StreamHandle,
    audio_handle: Option<AudioStreamHandle>,
    #[cfg(not(feature = "cursor"))] cursor_handle: Option<
        snow_cursor_capture::CursorStreamHandle,
    >,
) -> (StreamMultiplexer<RecordingEvent>, Vec<SourceId>) {
    let mut active_sources = Vec::new();

    let mut builder = StreamMultiplexerBuilder::new(MultiplexerConfig {
        select_timeout: Duration::from_millis(25),
        priority_source: Some(AUDIO_SOURCE),
        priority_drain_batch: 8,
        output_capacity: 16,
        output_send_timeout: Duration::from_millis(10),
    });

    // --- Video source ---
    // When cursor feature is enabled, the video mapper extracts embedded
    // cursor data and emits both Video and Cursor events (one-to-many).
    builder.register(
        SourceConfig {
            source_id: VIDEO_SOURCE,
            channel_capacity: VIDEO_CHANNEL_CAPACITY,
            send_timeout: SOURCE_SEND_TIMEOUT,
        },
        video_handle,
        video_mapper,
    );
    active_sources.push(VIDEO_SOURCE);

    // When cursor feature is enabled, cursor events come from the video
    // mapper, so we also track CURSOR_SOURCE as active.
    #[cfg(feature = "cursor")]
    active_sources.push(CURSOR_SOURCE);

    // --- Audio source ---
    if let Some(handle) = audio_handle {
        builder.register(
            SourceConfig {
                source_id: AUDIO_SOURCE,
                channel_capacity: AUDIO_CHANNEL_CAPACITY,
                send_timeout: SOURCE_SEND_TIMEOUT,
            },
            handle,
            audio_mapper,
        );
        active_sources.push(AUDIO_SOURCE);
    }

    // --- Standalone cursor source (only when cursor feature is disabled) ---
    #[cfg(not(feature = "cursor"))]
    if let Some(handle) = cursor_handle {
        builder.register(
            SourceConfig {
                source_id: CURSOR_SOURCE,
                channel_capacity: CURSOR_CHANNEL_CAPACITY,
                send_timeout: SOURCE_SEND_TIMEOUT,
            },
            handle,
            cursor_mapper,
        );
        active_sources.push(CURSOR_SOURCE);
    }

    (builder.build(), active_sources)
}

/// Video mapper: converts `TaggedEvent<CaptureEvent>` into `RecordingEvent` events.
///
/// When the `cursor` feature is enabled, this is a one-to-many mapper that
/// extracts embedded cursor data from frames and emits both video and cursor
/// events. Lifecycle events (Paused, Resumed, StreamEnded, Error) also produce
/// corresponding cursor events to maintain the cursor source lifecycle contract.
///
/// When the `cursor` feature is disabled, this is a simple 1:1 mapper.
fn video_mapper(te: TaggedEvent<CaptureEvent>) -> SmallVec<[RecordingEvent; 2]> {
    let mut out = SmallVec::new();

    #[cfg(feature = "cursor")]
    {
        // Extract cursor lifecycle/data events before moving te.
        let cursor_event: Option<CursorEvent> = match &te.event {
            CaptureEvent::Frame(frame) => {
                frame.metadata.cursor.as_ref().map(|cursor_data| {
                    // Use frame timestamp; fall back to Instant::now() if absent.
                    let ts = frame
                        .metadata
                        .stream_timestamp
                        .clone()
                        .unwrap_or_else(|| StreamTimestamp {
                            instant: std::time::Instant::now(),
                            raw_os_ticks: None,
                            tick_format: TickFormat::RawQpc,
                        });
                    CursorEvent::Sample {
                        sample: cursor_data.clone(),
                        stream_timestamp: ts,
                    }
                })
            }
            CaptureEvent::Paused { at } => Some(CursorEvent::Paused { at: *at }),
            CaptureEvent::Resumed { at, gap } => {
                Some(CursorEvent::Resumed { at: *at, gap: *gap })
            }
            CaptureEvent::StreamEnded => Some(CursorEvent::StreamEnded),
            CaptureEvent::Error(_) => Some(CursorEvent::Error(CursorCaptureError::platform(
                "video stream failed",
            ))),
            // FrameDropped, ResolutionChanged: no cursor event
            _ => None,
        };

        if let Some(ce) = cursor_event {
            out.push(RecordingEvent::Cursor(TaggedEvent {
                source: CURSOR_SOURCE,
                event: ce,
            }));
        }
    }

    // Move the original tagged event — no clone of frame data.
    out.push(RecordingEvent::Video(te));
    out
}

/// Audio mapper: simple 1:1 wrapping in `RecordingEvent::Audio`.
fn audio_mapper(te: TaggedEvent<snow_audio_recorder::AudioEvent>) -> SmallVec<[RecordingEvent; 2]> {
    smallvec![RecordingEvent::Audio(te)]
}

/// Standalone cursor mapper: simple 1:1 wrapping in `RecordingEvent::Cursor`.
///
/// Only used when the `cursor` feature is disabled and a standalone
/// `CursorStreamHandle` is registered.
#[cfg(not(feature = "cursor"))]
fn cursor_mapper(te: TaggedEvent<CursorEvent>) -> SmallVec<[RecordingEvent; 2]> {
    smallvec![RecordingEvent::Cursor(te)]
}
