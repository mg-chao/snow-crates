//! Integration tests for compile-time constraints.
//!
//! These tests verify crate independence, feature flag hygiene, and
//! backward-compatible deprecated type aliases.
//!

/// Verify that `snow_capture::FrameTimestampAnchor` is the same type as
/// `snow_core::timestamp::TimestampAnchor`. If this test compiles and the
/// `TypeId` assertion holds, the alias is correct.
#[test]
#[allow(deprecated)]
fn deprecated_frame_timestamp_anchor_resolves_to_core() {
    use std::any::TypeId;
    assert_eq!(
        TypeId::of::<snow_capture::FrameTimestampAnchor>(),
        TypeId::of::<snow_core::timestamp::TimestampAnchor>(),
        "FrameTimestampAnchor must be an alias for snow_core::TimestampAnchor"
    );
}

/// Verify that `snow_audio_recorder::AudioTimestampAnchor` is the same type
/// as `snow_core::timestamp::TimestampAnchor`.
#[test]
#[allow(deprecated)]
fn deprecated_audio_timestamp_anchor_resolves_to_core() {
    use std::any::TypeId;
    assert_eq!(
        TypeId::of::<snow_audio_recorder::AudioTimestampAnchor>(),
        TypeId::of::<snow_core::timestamp::TimestampAnchor>(),
        "AudioTimestampAnchor must be an alias for snow_core::TimestampAnchor"
    );
}


/// Verify that `snow_capture::StreamHandle` (the concrete type) is still
/// publicly accessible. This is a compile-time-only check; we just need
/// the function to exist and reference the type.
#[test]
fn snow_capture_stream_handle_type_exists() {
    fn _assert_type_exists(_: &snow_capture::StreamHandle) {}
}

/// Verify that `snow_audio_recorder::AudioStreamHandle` is still publicly
/// accessible.
#[test]
fn snow_audio_recorder_stream_handle_type_exists() {
    fn _assert_type_exists(_: &snow_audio_recorder::AudioStreamHandle) {}
}
