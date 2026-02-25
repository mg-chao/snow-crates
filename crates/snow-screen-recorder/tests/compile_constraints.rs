//! Integration tests for compile-time constraints.
//!
//! These tests verify crate independence, feature flag hygiene, and
//! core type accessibility.

/// Verify that `snow_core::timestamp::TimestampAnchor` is publicly accessible.
#[test]
fn core_timestamp_anchor_type_exists() {
    fn _assert_type_exists(_: &snow_core::timestamp::TimestampAnchor) {}
}

/// Verify that `snow_capture::StreamHandle` (the concrete type) is still
/// publicly accessible.
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

/// Verify that `FrameMetadata.stream_timestamp` is accessible (non-deprecated path).
#[test]
fn frame_metadata_stream_timestamp_accessible() {
    let meta = snow_capture::FrameMetadata::default();
    let _ = meta.stream_timestamp;
}

/// Verify that `AudioTimestampAnchorExt` trait is accessible.
#[test]
fn audio_timestamp_anchor_ext_accessible() {
    fn _assert_trait_exists<T: snow_audio_recorder::AudioTimestampAnchorExt>() {}
}
