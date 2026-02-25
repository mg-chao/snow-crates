//! Event abstractions: `StreamEvent`, `SourceId`, and `TaggedEvent<T>`.
//!
//! These types provide a common interface for lifecycle introspection and
//! source-tagged event wrapping across all leaf crates.

use crate::timestamp::StreamTimestamp;

/// Trait for stream events that support lifecycle introspection and timestamp access.
///
/// Leaf event types (`CaptureEvent`, `AudioEvent`, `CursorEvent`) implement this
/// trait so the orchestrator can handle lifecycle and timestamp behavior uniformly.
///
/// All methods have default implementations returning `false` / `None`, so
/// implementors only need to override the relevant methods.
pub trait StreamEvent: Send + 'static {
    /// Returns `true` if this event represents a stream pause.
    fn is_paused(&self) -> bool {
        false
    }

    /// Returns `true` if this event represents a stream resume.
    fn is_resumed(&self) -> bool {
        false
    }

    /// Returns `true` if this event represents the terminal event for the source.
    fn is_stream_ended(&self) -> bool {
        false
    }

    /// Returns `true` if this event represents an error.
    fn is_error(&self) -> bool {
        false
    }

    /// Returns the timestamp associated with this event, if any.
    fn timestamp(&self) -> Option<&StreamTimestamp> {
        None
    }
}

/// A recorder-assigned source identifier used to tag stream events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u8);

/// A source-tagged wrapper that pairs a [`SourceId`] with an event payload.
///
/// Preserves the original payload type without field-by-field reconstruction.
/// When `T: StreamEvent`, all lifecycle and timestamp queries delegate to the
/// inner event.
#[derive(Clone, Debug)]
pub struct TaggedEvent<T> {
    /// The source that produced this event.
    pub source: SourceId,
    /// The original event payload.
    pub event: T,
}

impl<T: StreamEvent> StreamEvent for TaggedEvent<T> {
    fn is_paused(&self) -> bool {
        self.event.is_paused()
    }

    fn is_resumed(&self) -> bool {
        self.event.is_resumed()
    }

    fn is_stream_ended(&self) -> bool {
        self.event.is_stream_ended()
    }

    fn is_error(&self) -> bool {
        self.event.is_error()
    }

    fn timestamp(&self) -> Option<&StreamTimestamp> {
        self.event.timestamp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timestamp::{StreamTimestamp, TickFormat};
    use proptest::prelude::*;
    use std::time::Instant;

    /// A mock event implementing `StreamEvent` with configurable lifecycle state
    /// and optional timestamp, used for property-based testing.
    #[derive(Clone, Debug)]
    struct MockEvent {
        paused: bool,
        resumed: bool,
        stream_ended: bool,
        error: bool,
        timestamp: Option<StreamTimestamp>,
    }

    impl StreamEvent for MockEvent {
        fn is_paused(&self) -> bool {
            self.paused
        }
        fn is_resumed(&self) -> bool {
            self.resumed
        }
        fn is_stream_ended(&self) -> bool {
            self.stream_ended
        }
        fn is_error(&self) -> bool {
            self.error
        }
        fn timestamp(&self) -> Option<&StreamTimestamp> {
            self.timestamp.as_ref()
        }
    }

    /// Strategy to generate an arbitrary `StreamTimestamp`.
    fn arb_stream_timestamp() -> impl Strategy<Value = StreamTimestamp> {
        (
            prop::bool::ANY,       // use_raw_ticks
            any::<i64>(),          // raw_os_ticks value
            prop::bool::ANY,       // tick_format selector
        )
            .prop_map(|(use_raw, ticks, is_hns100)| StreamTimestamp {
                instant: Instant::now(),
                raw_os_ticks: if use_raw { Some(ticks) } else { None },
                tick_format: if is_hns100 {
                    TickFormat::Hns100
                } else {
                    TickFormat::RawQpc
                },
            })
    }

    /// Strategy to generate an arbitrary `MockEvent`.
    fn arb_mock_event() -> impl Strategy<Value = MockEvent> {
        (
            prop::bool::ANY,
            prop::bool::ANY,
            prop::bool::ANY,
            prop::bool::ANY,
            prop::option::of(arb_stream_timestamp()),
        )
            .prop_map(|(paused, resumed, ended, error, ts)| MockEvent {
                paused,
                resumed,
                stream_ended: ended,
                error,
                timestamp: ts,
            })
    }

    mod prop_tests {
        use super::*;

        /// A simple payload type with `PartialEq` for round-trip identity testing.
        /// Uses basic types to verify that `TaggedEvent` preserves the payload as-is.
        #[derive(Clone, Debug, PartialEq)]
        struct RoundTripPayload {
            id: u64,
            label: String,
            values: Vec<i32>,
        }

        /// Strategy to generate an arbitrary `RoundTripPayload`.
        fn arb_round_trip_payload() -> impl Strategy<Value = RoundTripPayload> {
            (
                any::<u64>(),
                "[a-zA-Z0-9]{0,20}",
                prop::collection::vec(any::<i32>(), 0..10),
            )
                .prop_map(|(id, label, values)| RoundTripPayload { id, label, values })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            /// **Validates: Requirements 4.2**
            ///
            /// Property 5: TaggedEvent payload round-trip.
            /// For any payload of type T, wrapping it in TaggedEvent<T> and
            /// accessing `.event` yields a value identical to the original.
            /// No field-by-field reconstruction occurs.
            #[test]
            fn prop_tagged_event_payload_round_trip(
                source_id in any::<u8>(),
                payload in arb_round_trip_payload(),
            ) {
                let original = payload.clone();
                let tagged = TaggedEvent {
                    source: SourceId(source_id),
                    event: payload,
                };

                // Accessing .event must yield the exact original payload
                prop_assert_eq!(
                    &tagged.event,
                    &original,
                    "TaggedEvent payload round-trip failed: .event differs from original"
                );

                // Source ID must also be preserved
                prop_assert_eq!(
                    tagged.source,
                    SourceId(source_id),
                    "TaggedEvent source round-trip failed"
                );
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            /// **Validates: Requirements 4.4**
            ///
            /// Property 4: TaggedEvent delegates StreamEvent faithfully.
            /// For any event implementing StreamEvent, wrapping it in TaggedEvent
            /// produces identical results for all StreamEvent methods.
            #[test]
            fn prop_tagged_event_delegates_stream_event(
                source_id in any::<u8>(),
                mock in arb_mock_event(),
            ) {
                let tagged = TaggedEvent {
                    source: SourceId(source_id),
                    event: mock.clone(),
                };

                // All five StreamEvent methods must delegate faithfully
                prop_assert_eq!(
                    tagged.is_paused(),
                    mock.is_paused(),
                    "is_paused mismatch"
                );
                prop_assert_eq!(
                    tagged.is_resumed(),
                    mock.is_resumed(),
                    "is_resumed mismatch"
                );
                prop_assert_eq!(
                    tagged.is_stream_ended(),
                    mock.is_stream_ended(),
                    "is_stream_ended mismatch"
                );
                prop_assert_eq!(
                    tagged.is_error(),
                    mock.is_error(),
                    "is_error mismatch"
                );

                // Timestamp delegation: both should be Some or both None,
                // and when Some the instant and tick_format must match.
                match (tagged.timestamp(), mock.timestamp()) {
                    (Some(t), Some(m)) => {
                        prop_assert_eq!(t.instant, m.instant, "timestamp instant mismatch");
                        prop_assert_eq!(t.raw_os_ticks, m.raw_os_ticks, "raw_os_ticks mismatch");
                        prop_assert_eq!(t.tick_format, m.tick_format, "tick_format mismatch");
                    }
                    (None, None) => {} // both None — correct
                    _ => prop_assert!(false, "timestamp Some/None mismatch"),
                }
            }
        }
    }
}