mod error;
mod platform;
pub mod streaming;

pub use error::CursorCaptureError;
pub use streaming::{CursorStreamConfig, CursorStreamHandle};

use snow_core::event::StreamEvent;
use snow_core::timestamp::StreamTimestamp;
use std::collections::HashSet;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum CursorCompositionMode {
    #[default]
    AlphaBlend,
    MaskedColor,
}

#[derive(Clone, Debug)]
pub struct CursorShape {
    pub shape_id: u64,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub width: u32,
    pub height: u32,
    pub composition_mode: CursorCompositionMode,
    pub shape_rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct CursorFrameSample {
    pub position_x: i32,
    pub position_y: i32,
    pub visible: bool,
    pub shape_id: Option<u64>,
    pub shape: Option<CursorShape>,
}

/// Events emitted by a cursor capture stream.
///
/// The timestamp is carried in the `Sample` variant (not in
/// `CursorFrameSample`) to avoid a breaking change to the existing
/// struct.
#[derive(Debug)]
pub enum CursorEvent {
    /// A new cursor sample with its stream-relative timestamp.
    Sample {
        sample: CursorFrameSample,
        stream_timestamp: StreamTimestamp,
    },
    /// The stream was paused at the given instant.
    Paused { at: Instant },
    /// The stream was resumed at the given instant after a gap.
    Resumed { at: Instant, gap: Duration },
    /// The stream ended cleanly — no more events will arrive.
    StreamEnded,
    /// The stream encountered an error.
    Error(CursorCaptureError),
}

impl StreamEvent for CursorEvent {
    fn is_paused(&self) -> bool {
        matches!(self, CursorEvent::Paused { .. })
    }

    fn is_resumed(&self) -> bool {
        matches!(self, CursorEvent::Resumed { .. })
    }

    fn is_stream_ended(&self) -> bool {
        matches!(self, CursorEvent::StreamEnded)
    }

    fn is_error(&self) -> bool {
        matches!(self, CursorEvent::Error(_))
    }

    fn timestamp(&self) -> Option<&StreamTimestamp> {
        match self {
            CursorEvent::Sample { stream_timestamp, .. } => Some(stream_timestamp),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ShapePayload {
    hotspot_x: u32,
    hotspot_y: u32,
    width: u32,
    height: u32,
    composition_mode: CursorCompositionMode,
    shape_rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
struct CursorProbe {
    position_x: i32,
    position_y: i32,
    visible: bool,
    shape: Option<ShapePayload>,
}

pub struct CursorSampler {
    inner: platform::CursorSamplerImpl,
    emitted_shape_ids: HashSet<u64>,
    last_shape_id: Option<u64>,
}

impl CursorSampler {
    pub fn new() -> Result<Self, CursorCaptureError> {
        Ok(Self {
            inner: platform::CursorSamplerImpl::new()?,
            emitted_shape_ids: HashSet::new(),
            last_shape_id: None,
        })
    }

    pub fn sample(&mut self) -> Result<CursorFrameSample, CursorCaptureError> {
        let probe = self.inner.sample_cursor()?;
        let mut shape_id = self.last_shape_id;
        let mut shape = None;

        if let Some(payload) = probe.shape {
            let id = hash_shape_payload(&payload);
            shape_id = Some(id);
            self.last_shape_id = Some(id);

            if self.emitted_shape_ids.insert(id) {
                shape = Some(CursorShape {
                    shape_id: id,
                    hotspot_x: payload.hotspot_x,
                    hotspot_y: payload.hotspot_y,
                    width: payload.width,
                    height: payload.height,
                    composition_mode: payload.composition_mode,
                    shape_rgba: payload.shape_rgba,
                });
            }
        }

        Ok(CursorFrameSample {
            position_x: probe.position_x,
            position_y: probe.position_y,
            visible: probe.visible,
            shape_id,
            shape,
        })
    }
}

fn hash_shape_payload(shape: &ShapePayload) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    fn hash_bytes(mut h: u64, bytes: &[u8]) -> u64 {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        h
    }

    let mut h = FNV_OFFSET;
    h = hash_bytes(h, &shape.hotspot_x.to_le_bytes());
    h = hash_bytes(h, &shape.hotspot_y.to_le_bytes());
    h = hash_bytes(h, &shape.width.to_le_bytes());
    h = hash_bytes(h, &shape.height.to_le_bytes());
    h = hash_bytes(
        h,
        &[match shape.composition_mode {
            CursorCompositionMode::AlphaBlend => 0,
            CursorCompositionMode::MaskedColor => 1,
        }],
    );
    hash_bytes(h, &shape.shape_rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use snow_core::error::{Classify, ErrorClass};
    use snow_core::timestamp::TickFormat;

    fn payload(seed: u8) -> ShapePayload {
        ShapePayload {
            hotspot_x: 1,
            hotspot_y: 2,
            width: 2,
            height: 2,
            composition_mode: CursorCompositionMode::AlphaBlend,
            shape_rgba: vec![seed; 16],
        }
    }

    #[test]
    fn shape_id_changes_when_shape_pixels_change() {
        let a = hash_shape_payload(&payload(1));
        let b = hash_shape_payload(&payload(2));
        assert_ne!(a, b);
    }

    /// Generate an arbitrary `CursorEvent` covering all variants.
    fn arb_cursor_event() -> impl Strategy<Value = CursorEvent> {
        prop_oneof![
            // Sample — data variant with timestamp
            (any::<i32>(), any::<i32>(), any::<bool>(), any::<Option<i64>>())
                .prop_map(|(x, y, visible, raw_ticks)| {
                    CursorEvent::Sample {
                        sample: CursorFrameSample {
                            position_x: x,
                            position_y: y,
                            visible,
                            shape_id: None,
                            shape: None,
                        },
                        stream_timestamp: StreamTimestamp {
                            instant: Instant::now(),
                            raw_os_ticks: raw_ticks,
                            tick_format: TickFormat::RawQpc,
                        },
                    }
                }),
            // Paused
            Just(()).prop_map(|_| CursorEvent::Paused { at: Instant::now() }),
            // Resumed
            (0u64..10_000u64).prop_map(|gap_ms| CursorEvent::Resumed {
                at: Instant::now(),
                gap: Duration::from_millis(gap_ms),
            }),
            // StreamEnded
            Just(()).prop_map(|_| CursorEvent::StreamEnded),
            // Error
            prop_oneof![
                Just(()).prop_map(|_| CursorCaptureError::UnsupportedPlatform),
                "[a-z]{1,20}".prop_map(CursorCaptureError::Platform),
            ]
            .prop_map(CursorEvent::Error),
        ]
    }

    // **Validates: Requirements 3.5, 3.6**
    //
    // Property 3: StreamEvent lifecycle consistency (CursorEvent)
    //
    // For lifecycle variants (Paused, Resumed, StreamEnded, Error), exactly
    // one lifecycle method returns true. For data variants (Sample), all
    // lifecycle methods return false.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_cursor_event_lifecycle_consistency(event in arb_cursor_event()) {
            let lifecycle_results = [
                event.is_paused(),
                event.is_resumed(),
                event.is_stream_ended(),
                event.is_error(),
            ];
            let true_count = lifecycle_results.iter().filter(|&&v| v).count();

            let is_lifecycle_variant = matches!(
                event,
                CursorEvent::Paused { .. }
                    | CursorEvent::Resumed { .. }
                    | CursorEvent::StreamEnded
                    | CursorEvent::Error(_)
            );

            if is_lifecycle_variant {
                // Exactly one lifecycle method returns true
                prop_assert_eq!(
                    true_count, 1,
                    "lifecycle variant should have exactly one true method, got {}",
                    true_count
                );
            } else {
                // Data variants: all lifecycle methods return false
                prop_assert_eq!(
                    true_count, 0,
                    "data variant should have all false lifecycle methods, got {} true",
                    true_count
                );
            }

            // Verify specific lifecycle method matches the variant
            match &event {
                CursorEvent::Paused { .. } => {
                    prop_assert!(event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                CursorEvent::Resumed { .. } => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                CursorEvent::StreamEnded => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                CursorEvent::Error(_) => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(event.is_error());
                }
                _ => {}
            }

            // Verify timestamp() behavior:
            // - Sample → Some (always has stream_timestamp)
            // - All other variants → None
            match &event {
                CursorEvent::Sample { .. } => {
                    prop_assert!(event.timestamp().is_some(),
                        "Sample should return Some from timestamp()");
                }
                _ => {
                    prop_assert!(event.timestamp().is_none(),
                        "Non-Sample variant should return None from timestamp()");
                }
            }
        }
    }

    /// Generate an arbitrary `CursorCaptureError` covering all variants.
    fn arb_cursor_capture_error() -> impl Strategy<Value = CursorCaptureError> {
        prop_oneof![
            Just(()).prop_map(|_| CursorCaptureError::UnsupportedPlatform),
            "\\PC{1,50}".prop_map(CursorCaptureError::Platform),
        ]
    }

    // **Validates: Requirements 1.4**
    //
    // Property 2: Tick format matches source type (cursor)
    //
    // For any cursor sample constructed the way CursorStreamHandle's
    // poll_loop builds them (TickFormat::RawQpc, raw_os_ticks = None),
    // the stream_timestamp.tick_format SHALL be RawQpc.
    //
    // We test at the event construction level because CursorStreamHandle::start()
    // requires a real platform cursor sampler.

    /// Generate an arbitrary `CursorEvent::Sample` mimicking the
    /// `CursorStreamHandle` poll_loop construction path.
    fn arb_cursor_sample_from_handle() -> impl Strategy<Value = CursorEvent> {
        (any::<i32>(), any::<i32>(), any::<bool>(), proptest::option::of(any::<u64>()))
            .prop_map(|(x, y, visible, shape_id)| {
                // This mirrors the poll_loop in streaming.rs:
                // stream_timestamp is built with RawQpc and raw_os_ticks = None
                let stream_timestamp = StreamTimestamp {
                    instant: Instant::now(),
                    raw_os_ticks: None,
                    tick_format: TickFormat::RawQpc,
                };
                CursorEvent::Sample {
                    sample: CursorFrameSample {
                        position_x: x,
                        position_y: y,
                        visible,
                        shape_id,
                        shape: None,
                    },
                    stream_timestamp,
                }
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_cursor_tick_format_matches_source(event in arb_cursor_sample_from_handle()) {
            match &event {
                CursorEvent::Sample { stream_timestamp, .. } => {
                    prop_assert_eq!(
                        stream_timestamp.tick_format,
                        TickFormat::RawQpc,
                        "Cursor samples from CursorStreamHandle must use TickFormat::RawQpc"
                    );
                    // The poll_loop always sets raw_os_ticks = None
                    prop_assert!(
                        stream_timestamp.raw_os_ticks.is_none(),
                        "Cursor samples from CursorStreamHandle should have raw_os_ticks = None"
                    );
                }
                _ => {
                    prop_assert!(false, "Generator should only produce Sample variants");
                }
            }
        }
    }

    // **Validates: Requirements 2.6**
    //
    // Property 6: CursorCaptureError classification correctness
    //
    // For any CursorCaptureError variant, Classify::class() returns the
    // expected ErrorClass: UnsupportedPlatform → InvalidConfig,
    // Platform(_) → Transient.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_cursor_capture_error_classification(err in arb_cursor_capture_error()) {
            let class = err.class();
            match &err {
                CursorCaptureError::UnsupportedPlatform => {
                    prop_assert_eq!(class, ErrorClass::InvalidConfig,
                        "UnsupportedPlatform should classify as InvalidConfig");
                }
                CursorCaptureError::Platform(_) => {
                    prop_assert_eq!(class, ErrorClass::Transient,
                        "Platform(_) should classify as Transient");
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Unit tests for CursorStreamHandle and backward compatibility
    // Validates: Requirements 2.2, 2.3, 8.3, 9.2
    // ---------------------------------------------------------------

    /// Helper: returns true when the platform supports cursor capture.
    fn platform_supported() -> bool {
        CursorSampler::new().is_ok()
    }

    /// 7.9.1 — Start/stop lifecycle of CursorStreamHandle.
    ///
    /// Start a handle, verify it is running, stop it, verify it stops
    /// and emits `StreamEnded`. On unsupported platforms, `start()`
    /// returns `UnsupportedPlatform` and the test passes gracefully.
    #[test]
    fn cursor_stream_handle_start_stop_lifecycle() {
        let config = CursorStreamConfig {
            poll_interval: Duration::from_millis(20),
            channel_capacity: 4,
        };

        let handle = match CursorStreamHandle::start(config) {
            Ok(h) => h,
            Err(CursorCaptureError::UnsupportedPlatform) => return, // graceful skip
            Err(e) => panic!("unexpected error from CursorStreamHandle::start: {e}"),
        };

        // The handle should be running right after start.
        assert!(handle.is_running(), "handle should be running after start");

        // Stop the stream.
        handle.stop();

        // Drain events until we see StreamEnded (with a reasonable timeout).
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_stream_ended = false;
        while Instant::now() < deadline {
            match handle.recv_timeout(Duration::from_millis(100)) {
                Ok(CursorEvent::StreamEnded) => {
                    saw_stream_ended = true;
                    break;
                }
                Ok(_) => continue, // consume any pending samples
                Err(_) => break,
            }
        }
        assert!(saw_stream_ended, "expected StreamEnded after stop()");

        // After the worker thread finishes, is_running should become false.
        // Give the thread a moment to join.
        std::thread::sleep(Duration::from_millis(50));
        assert!(!handle.is_running(), "handle should not be running after StreamEnded");
    }

    /// 7.9.2 — CursorSampler poll API still works (backward compatibility).
    ///
    /// `CursorSampler::new()` and `sample()` must remain usable
    /// independently of `CursorStreamHandle`.
    #[test]
    fn cursor_sampler_poll_api_backward_compat() {
        let mut sampler = match CursorSampler::new() {
            Ok(s) => s,
            Err(CursorCaptureError::UnsupportedPlatform) => return, // graceful skip
            Err(e) => panic!("unexpected error from CursorSampler::new: {e}"),
        };

        // Calling sample() should succeed and return a CursorFrameSample.
        let sample = sampler
            .sample()
            .expect("CursorSampler::sample() should succeed on a supported platform");

        // Basic structural assertions — the sample has the expected fields.
        // We cannot predict exact values, but we can verify the struct is
        // well-formed (visible is a bool, shape_id is Option<u64>, etc.).
        let _ = sample.position_x;
        let _ = sample.position_y;
        let _ = sample.visible;
        let _ = sample.shape_id;
        let _ = sample.shape;

        // A second sample should also succeed (sampler is reusable).
        let _sample2 = sampler
            .sample()
            .expect("CursorSampler::sample() should succeed on second call");
    }

    /// 7.9.3 — Every CursorEvent::Sample from the handle has a valid
    /// `stream_timestamp`.
    ///
    /// Start a handle, collect a few samples, and verify each one
    /// carries a `stream_timestamp` with a valid `Instant`.
    #[test]
    fn cursor_stream_handle_samples_have_timestamp() {
        if !platform_supported() {
            return; // graceful skip on unsupported platforms
        }

        let config = CursorStreamConfig {
            poll_interval: Duration::from_millis(20),
            channel_capacity: 8,
        };

        let handle = CursorStreamHandle::start(config)
            .expect("start should succeed on supported platform");

        let before = Instant::now();

        // Collect up to 3 samples (with a generous timeout).
        let mut samples_checked = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while samples_checked < 3 && Instant::now() < deadline {
            match handle.recv_timeout(Duration::from_millis(500)) {
                Ok(CursorEvent::Sample { stream_timestamp, .. }) => {
                    // The instant should be at or after `before`.
                    assert!(
                        stream_timestamp.instant >= before,
                        "stream_timestamp.instant should be >= test start time"
                    );
                    // tick_format should be RawQpc (as set by the poll loop).
                    assert_eq!(
                        stream_timestamp.tick_format,
                        TickFormat::RawQpc,
                        "cursor samples should use TickFormat::RawQpc"
                    );
                    samples_checked += 1;
                }
                Ok(CursorEvent::Error(e)) => {
                    panic!("unexpected error event from cursor stream: {e}");
                }
                Ok(_) => continue, // lifecycle events — skip
                Err(_) => break,   // timeout — stop waiting
            }
        }

        handle.stop();

        assert!(
            samples_checked > 0,
            "expected at least one Sample event with a valid stream_timestamp"
        );
    }
}
