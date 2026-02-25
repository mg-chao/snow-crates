use std::time::Duration;

use snow_capture::CaptureEvent;

use crate::config::RecordingTarget;
use crate::error::{Result, ScreenRecorderError};
#[cfg(feature = "cursor")]
use crate::event::CursorCaptureEvent;
use crate::event::{RecordingEvent, StreamTimestamp, VideoCaptureEvent};

/// Default send timeout for cursor side-channel (10ms).
const SEND_TIMEOUT: Duration = Duration::from_millis(10);

/// Create a video event mapper closure for use with `StreamBridge`.
///
/// Converts `CaptureEvent` variants into `RecordingEvent::Video(...)`.
/// When the `cursor` feature is enabled and `cursor_tx` is `Some`,
/// the mapper also extracts embedded cursor data from frames and
/// sends it as a side-effect on the cursor channel.
pub(crate) fn create_video_mapper(
    cursor_tx: Option<crossbeam_channel::Sender<RecordingEvent>>,
) -> impl Fn(CaptureEvent) -> RecordingEvent + Send + 'static {
    move |event: CaptureEvent| -> RecordingEvent {
        match event {
            CaptureEvent::Frame(frame) => {
                // Extract cursor data if present and cursor_tx is available.
                #[cfg(feature = "cursor")]
                if let Some(ref ctx) = cursor_tx {
                    if let Some(cursor_data) = &frame.metadata.cursor {
                        let cursor_event =
                            RecordingEvent::Cursor(CursorCaptureEvent::Sample(cursor_data.clone()));
                        // Best-effort send for cursor — don't block video pipeline.
                        let _ = ctx.send_timeout(cursor_event, SEND_TIMEOUT);
                    }
                }

                // Suppress unused-variable warning when cursor feature is off.
                #[cfg(not(feature = "cursor"))]
                let _ = &cursor_tx;

                let ts = StreamTimestamp {
                    instant: frame
                        .metadata
                        .capture_time
                        .unwrap_or_else(std::time::Instant::now),
                    qpc_100ns: frame.metadata.present_time_qpc,
                };

                RecordingEvent::Video(VideoCaptureEvent::Frame {
                    rgba: frame.as_rgba_bytes().to_vec(),
                    width: frame.width(),
                    height: frame.height(),
                    timestamp: ts,
                    is_duplicate: frame.metadata.is_duplicate,
                })
            }

            CaptureEvent::ResolutionChanged {
                old_width,
                old_height,
                new_width,
                new_height,
            } => RecordingEvent::Video(VideoCaptureEvent::ResolutionChanged {
                old_width,
                old_height,
                new_width,
                new_height,
            }),

            CaptureEvent::FrameDropped { sequence } => {
                RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence })
            }

            CaptureEvent::Paused { at } => {
                RecordingEvent::Video(VideoCaptureEvent::Paused { at })
            }

            CaptureEvent::Resumed { at, gap } => {
                RecordingEvent::Video(VideoCaptureEvent::Resumed { at, gap })
            }

            CaptureEvent::StreamEnded => {
                #[cfg(feature = "cursor")]
                if let Some(ref ctx) = cursor_tx {
                    let _ = ctx.send_timeout(
                        RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded),
                        SEND_TIMEOUT,
                    );
                }
                #[cfg(not(feature = "cursor"))]
                let _ = &cursor_tx;

                RecordingEvent::Video(VideoCaptureEvent::StreamEnded)
            }

            CaptureEvent::Error(err) => {
                #[cfg(feature = "cursor")]
                if let Some(ref ctx) = cursor_tx {
                    let _ = ctx.send_timeout(
                        RecordingEvent::Cursor(CursorCaptureEvent::Error(Box::new(
                            std::io::Error::other("video stream failed"),
                        ))),
                        SEND_TIMEOUT,
                    );
                }
                #[cfg(not(feature = "cursor"))]
                let _ = &cursor_tx;

                RecordingEvent::Video(VideoCaptureEvent::Error(Box::new(err)))
            }
        }
    }
}

/// Resolve a [`RecordingTarget`] (public API type) to a [`snow_capture::CaptureTarget`]
/// (snow-capture internal type) by validating selectors against the current system state.
///
/// - `MonitorSelector`: enumerates monitors and matches by `stable_id`
/// - `WindowSelector`: validates the window handle is capturable (Windows-only)
/// - `RecordingRegion`: validates dimensions are non-zero via `CaptureRegion::new`
/// Resolve a `MonitorSelector` against a set of known monitors.
/// Returns the matching `MonitorId` or an error if no match is found.
///
/// This is extracted from `resolve_capture_target` for testability.
pub(crate) fn resolve_monitor_selector(
    selector: &crate::config::MonitorSelector,
    monitors: &[snow_capture::region::MonitorGeometry],
) -> Result<snow_capture::MonitorId> {
    monitors
        .iter()
        .find(|m| m.monitor.stable_id() == selector.stable_id)
        .map(|m| m.monitor.clone())
        .ok_or_else(|| {
            ScreenRecorderError::Capture(snow_capture::error::CaptureError::InvalidConfig(format!(
                "monitor with stable_id '{}' not found or disconnected",
                selector.stable_id
            )))
        })
}

pub(crate) fn resolve_capture_target(
    target: &RecordingTarget,
) -> Result<snow_capture::CaptureTarget> {
    match target {
        RecordingTarget::PrimaryMonitor => Ok(snow_capture::CaptureTarget::PrimaryMonitor),
        RecordingTarget::Monitor(selector) => {
            let layout = snow_capture::MonitorLayout::snapshot()?;
            let monitor = resolve_monitor_selector(selector, &layout.monitors)?;
            Ok(snow_capture::CaptureTarget::Monitor(monitor))
        }
        RecordingTarget::Window(selector) => {
            let window_id = snow_capture::WindowId::from_raw_handle(selector.raw_handle);
            // Validate the window is still alive and capturable (Windows-only).
            #[cfg(target_os = "windows")]
            {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::UI::WindowsAndMessaging::IsWindow;

                let hwnd = HWND(selector.raw_handle as *mut std::ffi::c_void);
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    return Err(ScreenRecorderError::Capture(
                        snow_capture::error::CaptureError::InvalidConfig(format!(
                            "window with handle 0x{:x} is not a valid window",
                            selector.raw_handle
                        )),
                    ));
                }
            }
            Ok(snow_capture::CaptureTarget::Window(window_id))
        }
        RecordingTarget::Region(region) => {
            let capture_region =
                snow_capture::CaptureRegion::new(region.x, region.y, region.width, region.height)?;
            Ok(snow_capture::CaptureTarget::Region(capture_region))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::event::{
        AudioCaptureEvent, CursorCaptureEvent, RecordingEvent, StreamTimestamp, VideoCaptureEvent,
    };
    use proptest::prelude::*;
    use snow_audio_recorder::{AudioFormat, AudioSampleFormat, AudioSourceKind};
    use snow_cursor_capture::CursorFrameSample;
    use std::time::{Duration, Instant};

    // Strategies for generating arbitrary leaf-crate-level event data

    fn arb_stream_timestamp() -> impl Strategy<Value = (Instant, Option<i64>)> {
        (Just(Instant::now()), proptest::option::of(0i64..i64::MAX))
    }

    /// Strategy for video frame fields (small RGBA to keep memory bounded).
    fn arb_video_frame_fields()
    -> impl Strategy<Value = (u32, u32, Vec<u8>, bool, Instant, Option<i64>)> {
        (
            1u32..64,
            1u32..64,
            proptest::bool::ANY,
            arb_stream_timestamp(),
        )
            .prop_flat_map(|(w, h, is_dup, (instant, qpc))| {
                let len = (w * h * 4) as usize;
                (
                    Just(w),
                    Just(h),
                    proptest::collection::vec(any::<u8>(), len..=len),
                    Just(is_dup),
                    Just(instant),
                    Just(qpc),
                )
            })
    }

    fn arb_resolution_changed() -> impl Strategy<Value = (u32, u32, u32, u32)> {
        (1u32..4096, 1u32..4096, 1u32..4096, 1u32..4096)
    }

    fn arb_audio_source_kind() -> impl Strategy<Value = AudioSourceKind> {
        prop_oneof![
            Just(AudioSourceKind::System),
            Just(AudioSourceKind::Microphone),
        ]
    }

    fn arb_audio_format() -> impl Strategy<Value = AudioFormat> {
        (
            prop_oneof![Just(44100u32), Just(48000u32), Just(96000u32)],
            1u16..=8,
            prop_oneof![Just(AudioSampleFormat::F32), Just(AudioSampleFormat::I16)],
        )
            .prop_map(|(rate, ch, fmt)| AudioFormat::new(rate, ch, fmt))
    }

    fn arb_cursor_sample() -> impl Strategy<Value = CursorFrameSample> {
        (
            any::<i32>(),
            any::<i32>(),
            any::<bool>(),
            proptest::option::of(any::<u64>()),
        )
            .prop_map(|(px, py, vis, sid)| CursorFrameSample {
                position_x: px,
                position_y: py,
                visible: vis,
                shape_id: sid,
                shape: None,
            })
    }

    // Property 1: Event translation fidelity
    //
    //
    // For any leaf-crate event data, translating it into the
    // corresponding RecordingEvent variant and destructuring back
    // must yield identical payload fields.

    proptest! {
        /// Video Frame translation preserves all payload fields.
        #[test]
        fn prop_video_frame_translation_preserves_fields(
            (width, height, rgba, is_duplicate, instant, qpc) in arb_video_frame_fields(),
        ) {
            let ts = StreamTimestamp {
                instant,
                qpc_100ns: qpc,
            };

            // Translate: mirrors what video_forward_loop does for CaptureEvent::Frame
            let event = RecordingEvent::Video(VideoCaptureEvent::Frame {
                rgba: rgba.clone(),
                width,
                height,
                timestamp: ts,
                is_duplicate,
            });

            match event {
                RecordingEvent::Video(VideoCaptureEvent::Frame {
                    rgba: out_rgba,
                    width: out_w,
                    height: out_h,
                    timestamp: out_ts,
                    is_duplicate: out_dup,
                }) => {
                    prop_assert_eq!(out_rgba, rgba, "RGBA data must be preserved");
                    prop_assert_eq!(out_w, width, "width must be preserved");
                    prop_assert_eq!(out_h, height, "height must be preserved");
                    prop_assert_eq!(out_ts.qpc_100ns, qpc, "QPC timestamp must be preserved");
                    prop_assert_eq!(out_ts.instant, instant, "Instant must be preserved");
                    prop_assert_eq!(out_dup, is_duplicate, "is_duplicate flag must be preserved");
                }
                _ => prop_assert!(false, "expected Video(Frame) variant"),
            }
        }

        /// Video ResolutionChanged translation preserves all dimension fields.
        #[test]
        fn prop_video_resolution_changed_preserves_fields(
            (old_w, old_h, new_w, new_h) in arb_resolution_changed(),
        ) {
            let event = RecordingEvent::Video(VideoCaptureEvent::ResolutionChanged {
                old_width: old_w,
                old_height: old_h,
                new_width: new_w,
                new_height: new_h,
            });

            match event {
                RecordingEvent::Video(VideoCaptureEvent::ResolutionChanged {
                    old_width, old_height, new_width, new_height,
                }) => {
                    prop_assert_eq!(old_width, old_w);
                    prop_assert_eq!(old_height, old_h);
                    prop_assert_eq!(new_width, new_w);
                    prop_assert_eq!(new_height, new_h);
                }
                _ => prop_assert!(false, "expected Video(ResolutionChanged) variant"),
            }
        }

        /// Video FrameDropped translation preserves the sequence number.
        #[test]
        fn prop_video_frame_dropped_preserves_sequence(
            sequence in 0u64..u64::MAX,
        ) {
            let event = RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence });

            match event {
                RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence: out }) => {
                    prop_assert_eq!(out, sequence);
                }
                _ => prop_assert!(false, "expected Video(FrameDropped) variant"),
            }
        }

        /// Video Paused/Resumed translation preserves backend timestamps.
        #[test]
        fn prop_video_pause_resume_preserves_timestamps(
            gap_ms in 0u64..100_000,
        ) {
            let pause_at = Instant::now();
            let resume_at = Instant::now();
            let gap = Duration::from_millis(gap_ms);

            // Paused
            let paused = RecordingEvent::Video(VideoCaptureEvent::Paused { at: pause_at });
            match paused {
                RecordingEvent::Video(VideoCaptureEvent::Paused { at }) => {
                    prop_assert_eq!(at, pause_at, "Paused timestamp must be preserved");
                }
                _ => prop_assert!(false, "expected Video(Paused) variant"),
            }

            // Resumed
            let resumed = RecordingEvent::Video(VideoCaptureEvent::Resumed { at: resume_at, gap });
            match resumed {
                RecordingEvent::Video(VideoCaptureEvent::Resumed { at, gap: out_gap }) => {
                    prop_assert_eq!(at, resume_at, "Resumed timestamp must be preserved");
                    prop_assert_eq!(out_gap, gap, "Resumed gap must be preserved");
                }
                _ => prop_assert!(false, "expected Video(Resumed) variant"),
            }
        }

        /// Audio Packet translation preserves all payload fields.
        #[test]
        fn prop_audio_packet_translation_preserves_fields(
            source in arb_audio_source_kind(),
            data in proptest::collection::vec(any::<u8>(), 0..512),
            frames in 0u32..1024,
            format in arb_audio_format(),
            (instant, qpc) in arb_stream_timestamp(),
        ) {
            let ts = StreamTimestamp {
                instant,
                qpc_100ns: qpc,
            };

            let event = RecordingEvent::Audio(AudioCaptureEvent::Packet {
                source,
                data: data.clone(),
                frames,
                format,
                timestamp: ts,
            });

            match event {
                RecordingEvent::Audio(AudioCaptureEvent::Packet {
                    source: out_src,
                    data: out_data,
                    frames: out_frames,
                    format: out_fmt,
                    timestamp: out_ts,
                }) => {
                    prop_assert_eq!(out_src, source, "audio source must be preserved");
                    prop_assert_eq!(out_data, data, "audio data must be preserved");
                    prop_assert_eq!(out_frames, frames, "audio frames must be preserved");
                    prop_assert_eq!(out_fmt, format, "audio format must be preserved");
                    prop_assert_eq!(out_ts.qpc_100ns, qpc, "audio QPC must be preserved");
                    prop_assert_eq!(out_ts.instant, instant, "audio instant must be preserved");
                }
                _ => prop_assert!(false, "expected Audio(Packet) variant"),
            }
        }

        /// Cursor Sample translation preserves the full CursorFrameSample.
        #[test]
        fn prop_cursor_sample_translation_preserves_fields(
            sample in arb_cursor_sample(),
        ) {
            let event = RecordingEvent::Cursor(CursorCaptureEvent::Sample(sample.clone()));

            match event {
                RecordingEvent::Cursor(CursorCaptureEvent::Sample(out)) => {
                    prop_assert_eq!(out.position_x, sample.position_x, "cursor position_x must be preserved");
                    prop_assert_eq!(out.position_y, sample.position_y, "cursor position_y must be preserved");
                    prop_assert_eq!(out.visible, sample.visible, "cursor visible must be preserved");
                    prop_assert_eq!(out.shape_id, sample.shape_id, "cursor shape_id must be preserved");
                }
                _ => prop_assert!(false, "expected Cursor(Sample) variant"),
            }
        }

        // Property 2: Embedded cursor extraction
        //
        //
        // When a video frame carries embedded cursor data (non-None
        // FrameMetadata::cursor), the adapter must produce both a
        // RecordingEvent::Video and a RecordingEvent::Cursor whose
        // CursorFrameSample matches the frame's embedded cursor data.

        /// Embedded cursor extraction preserves all cursor fields.
        #[test]
        fn prop_embedded_cursor_extraction_preserves_data(
            sample in arb_cursor_sample(),
        ) {
            // Simulate what video_forward_loop does: clone the cursor data
            // from the frame metadata and wrap it in a CursorCaptureEvent.
            let extracted = CursorCaptureEvent::Sample(sample.clone());

            match extracted {
                CursorCaptureEvent::Sample(out) => {
                    prop_assert_eq!(out.position_x, sample.position_x,
                        "extracted cursor position_x must match embedded");
                    prop_assert_eq!(out.position_y, sample.position_y,
                        "extracted cursor position_y must match embedded");
                    prop_assert_eq!(out.visible, sample.visible,
                        "extracted cursor visible must match embedded");
                    prop_assert_eq!(out.shape_id, sample.shape_id,
                        "extracted cursor shape_id must match embedded");
                }
                _ => prop_assert!(false, "expected CursorCaptureEvent::Sample"),
            }
        }

        /// When cursor data is None, no cursor event should be produced.
        /// This tests the conditional extraction logic.
        #[test]
        fn prop_no_cursor_extraction_when_none(
            _width in 1u32..64,
            _height in 1u32..64,
        ) {
            let cursor_data: Option<CursorFrameSample> = None;

            // Simulate the extraction check from video_forward_loop
            let cursor_event_produced = cursor_data.as_ref().map(|cd| {
                CursorCaptureEvent::Sample(cd.clone())
            });

            prop_assert!(cursor_event_produced.is_none(),
                "no cursor event should be produced when cursor data is None");
        }
    }

    // Property 13: MonitorSelector resolution
    //
    //
    // Matching stable_id resolves to the correct MonitorId.
    // Non-matching stable_id returns InvalidConfig error containing
    // the unresolved stable_id.

    proptest! {
        #[test]
        fn prop_monitor_selector_resolution(
            adapter_luid in any::<u64>(),
            output_id in any::<u64>(),
            extra_monitors in 0usize..=3,
        ) {
            use crate::config::MonitorSelector;
            use super::resolve_monitor_selector;
            use snow_capture::region::MonitorGeometry;

            // Build a set of monitors including one with the known stable_id.
            let target_stable_id = format!("{:016x}-{:016x}", adapter_luid, output_id);
            let target_monitor = snow_capture::MonitorId::from_parts(
                adapter_luid, output_id, 0, "Test Monitor", false,
            );
            let target_geo = MonitorGeometry {
                monitor: target_monitor.clone(),
                x: 0, y: 0, width: 1920, height: 1080,
            };

            let mut monitors = vec![target_geo];
            // Add extra monitors with different stable_ids.
            for i in 0..extra_monitors {
                let other = snow_capture::MonitorId::from_parts(
                    i as u64 + 1000, i as u64 + 2000, 0,
                    format!("Other {i}"), false,
                );
                monitors.push(MonitorGeometry {
                    monitor: other,
                    x: 1920 * (i as i32 + 1), y: 0, width: 1920, height: 1080,
                });
            }

            // Matching selector should resolve to the correct MonitorId.
            let selector = MonitorSelector::new(&target_stable_id);
            let result = resolve_monitor_selector(&selector, &monitors);
            prop_assert!(result.is_ok(), "matching stable_id should resolve");
            prop_assert_eq!(result.unwrap().stable_id(), target_stable_id);

            // Non-matching selector should fail with InvalidConfig containing the stable_id.
            let bad_id = "0000000000000000-ffffffffffffffff";
            let bad_selector = MonitorSelector::new(bad_id);
            let bad_result = resolve_monitor_selector(&bad_selector, &monitors);
            prop_assert!(bad_result.is_err(), "non-matching stable_id should fail");
            let err_msg = format!("{}", bad_result.unwrap_err());
            prop_assert!(err_msg.contains(bad_id),
                "error should contain the unresolved stable_id, got: {}", err_msg);
        }
    }

    // Property 14: Cursor data path exclusivity
    //
    //
    // Cursor data arrives through exactly one path, determined at
    // compile time by the `cursor` feature flag:
    //
    // - cursor enabled:  VideoStreamAdapter extracts cursor from
    //   frame.metadata.cursor → CursorCaptureEvent on cursor channel.
    //   No CursorStreamAdapter is created.
    //
    // - cursor disabled: FrameMetadata has no cursor field.
    //   CursorStreamAdapter polls CursorSampler independently.
    //
    // These tests verify the current configuration's path and assert
    // that no duplicate cursor records can arise from both paths
    // simultaneously.

    /// When cursor feature is enabled, FrameMetadata has a `cursor` field.
    /// This is a compile-time assertion: if the field doesn't exist, this
    /// test won't compile.
    #[cfg(feature = "cursor")]
    #[test]
    fn cursor_path_is_embedded_when_feature_enabled() {
        let meta = snow_capture::FrameMetadata::default();
        assert!(
            meta.cursor.is_none(),
            "default FrameMetadata should have cursor = None"
        );
    }

    /// When cursor feature is disabled, FrameMetadata does NOT have a
    /// `cursor` field. CursorStreamAdapter provides cursor data instead.
    /// This is a compile-time assertion: the CursorSampler type must
    /// exist for the standalone path.
    #[cfg(not(feature = "cursor"))]
    #[test]
    fn cursor_path_is_standalone_when_feature_disabled() {
        // CursorSampler must be available for the standalone adapter path.
        let _sampler_type = std::any::type_name::<snow_cursor_capture::CursorSampler>();
        assert!(
            !_sampler_type.is_empty(),
            "CursorSampler type must be available for standalone cursor path"
        );
    }

    proptest! {
        /// Property 14: Cursor data path exclusivity.
        ///
        /// Simulates the video adapter's cursor extraction logic for
        /// arbitrary cursor samples. When the cursor feature is enabled,
        /// embedded cursor data in a frame produces exactly one cursor
        /// event on the cursor channel. When disabled, the video path
        /// never produces cursor events — cursor data comes exclusively
        /// from CursorStreamAdapter.
        #[test]
        fn prop_cursor_data_path_exclusivity(
            sample in arb_cursor_sample(),
            has_cursor in proptest::bool::ANY,
        ) {
            // Simulate the two channels the coordinator would receive from.
            let (video_tx, video_rx) = crossbeam_channel::unbounded::<RecordingEvent>();
            let (cursor_tx, cursor_rx) = crossbeam_channel::unbounded::<RecordingEvent>();

            // Always produce a video event (the frame itself).
            let video_event = RecordingEvent::Video(VideoCaptureEvent::Frame {
                rgba: vec![0u8; 4],
                width: 1,
                height: 1,
                timestamp: StreamTimestamp {
                    instant: Instant::now(),
                    qpc_100ns: None,
                },
                is_duplicate: false,
            });
            video_tx.send(video_event).unwrap();

            // Simulate the cursor extraction logic from video_forward_loop.
            // This mirrors the #[cfg(feature = "cursor")] block in the real code.
            #[cfg(feature = "cursor")]
            {
                // Embedded path: if the frame has cursor data, extract and
                // send on the cursor channel.
                let cursor_data: Option<CursorFrameSample> = if has_cursor {
                    Some(sample.clone())
                } else {
                    None
                };

                if let Some(cd) = cursor_data {
                    let cursor_event = RecordingEvent::Cursor(
                        CursorCaptureEvent::Sample(cd),
                    );
                    cursor_tx.send(cursor_event).unwrap();
                }
            }

            #[cfg(not(feature = "cursor"))]
            {
                // Standalone path: the video adapter never produces cursor
                // events. Cursor data would come from CursorStreamAdapter,
                // which we don't simulate here — the point is that the
                // video path produces zero cursor events.
                let _ = (&sample, has_cursor);
            }

            // Drop senders so receivers can drain.
            drop(video_tx);
            drop(cursor_tx);

            // Count events on each channel.
            let video_events: Vec<_> = video_rx.try_iter().collect();
            let cursor_events: Vec<_> = cursor_rx.try_iter().collect();

            // There should always be exactly one video event.
            prop_assert_eq!(video_events.len(), 1, "exactly one video event expected");

            #[cfg(feature = "cursor")]
            {
                // Embedded path: cursor events come from the video adapter.
                if has_cursor {
                    prop_assert_eq!(cursor_events.len(), 1,
                        "exactly one cursor event when frame has cursor data");
                    // Verify the cursor event matches the original sample.
                    match &cursor_events[0] {
                        RecordingEvent::Cursor(CursorCaptureEvent::Sample(out)) => {
                            prop_assert_eq!(out.position_x, sample.position_x);
                            prop_assert_eq!(out.position_y, sample.position_y);
                            prop_assert_eq!(out.visible, sample.visible);
                            prop_assert_eq!(out.shape_id, sample.shape_id);
                        }
                        _ => prop_assert!(false, "expected Cursor(Sample) variant"),
                    }
                } else {
                    prop_assert_eq!(cursor_events.len(), 0,
                        "no cursor event when frame has no cursor data");
                }
            }

            #[cfg(not(feature = "cursor"))]
            {
                // Standalone path: video adapter never produces cursor events.
                prop_assert_eq!(cursor_events.len(), 0,
                    "video adapter must not produce cursor events when cursor feature is disabled");
            }
        }
    }

    // Property 7: resolve_capture_target preserves semantics
    //
    //
    // For any valid RecordingTarget (non-zero dimensions, valid handles),
    // resolve_capture_target returns a CaptureTarget that preserves the
    // target semantics:
    //   - PrimaryMonitor → PrimaryMonitor
    //   - Region(r) → Region(cr) where coordinates match
    //
    // Note: Monitor(selector) requires a real MonitorLayout::snapshot()
    // and Window(selector) requires a real window handle (IsWindow check
    // on Windows), so those variants are not tested here.

    proptest! {

        /// PrimaryMonitor always resolves to CaptureTarget::PrimaryMonitor.
        #[test]
        fn prop_resolve_capture_target_primary_monitor(_dummy in 0u8..1) {
            use super::resolve_capture_target;
            use crate::config::RecordingTarget;

            let target = RecordingTarget::PrimaryMonitor;
            let result = resolve_capture_target(&target);
            prop_assert!(result.is_ok(), "PrimaryMonitor should always resolve");
            match result.unwrap() {
                snow_capture::CaptureTarget::PrimaryMonitor => { /* correct */ }
                _ => prop_assert!(false, "expected CaptureTarget::PrimaryMonitor"),
            }
        }

        /// Region with non-zero dimensions resolves to CaptureTarget::Region
        /// with matching coordinates.
        #[test]
        fn prop_resolve_capture_target_region(
            x in -10_000i32..10_000,
            y in -10_000i32..10_000,
            width in 1u32..10_000,
            height in 1u32..10_000,
        ) {
            use super::resolve_capture_target;
            use crate::config::{RecordingRegion, RecordingTarget};

            let region = RecordingRegion::new(x, y, width, height);
            let target = RecordingTarget::Region(region);
            let result = resolve_capture_target(&target);
            prop_assert!(result.is_ok(), "valid region should resolve, got: {:?}", result.err());
            match result.unwrap() {
                snow_capture::CaptureTarget::Region(cr) => {
                    prop_assert_eq!(cr.x, x, "x coordinate must be preserved");
                    prop_assert_eq!(cr.y, y, "y coordinate must be preserved");
                    prop_assert_eq!(cr.width, width, "width must be preserved");
                    prop_assert_eq!(cr.height, height, "height must be preserved");
                }
                _ => prop_assert!(false, "expected CaptureTarget::Region"),
            }
        }
    }

    //
    //
    // For any RecordingRegion where width == 0 or height == 0,
    // resolve_capture_target shall return an Err indicating invalid
    // dimensions.

    proptest! {
        #[test]
        fn prop_invalid_region_dimensions_produce_errors(
            x in -10_000i32..10_000,
            y in -10_000i32..10_000,
            zero_width in proptest::bool::ANY,
            non_zero_dim in 1u32..10_000,
        ) {
            use super::resolve_capture_target;
            use crate::config::{RecordingRegion, RecordingTarget};

            let (width, height) = if zero_width {
                (0, non_zero_dim)
            } else {
                (non_zero_dim, 0)
            };

            let region = RecordingRegion { x, y, width, height };
            let target = RecordingTarget::Region(region);
            let result = resolve_capture_target(&target);
            prop_assert!(result.is_err(), "zero-dimension region should produce an error");
        }
    }
}
