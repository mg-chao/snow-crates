//! Shared FFmpeg helpers used by both `recording` and `editing`.

use std::sync::OnceLock;

use ffmpeg_next as ffmpeg;

use crate::error::{Result, ScreenRecorderError};

/// Initialize FFmpeg exactly once. Thread-safe via `OnceLock`.
pub(crate) fn ensure_ffmpeg_initialized() -> Result<()> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| ffmpeg::init().map_err(|err| err.to_string()))
        .clone()
        .map_err(|err| {
            ScreenRecorderError::Encode(format!("failed to initialize ffmpeg: {err}"))
        })
}

/// Check whether an FFmpeg error is EAGAIN.
pub(crate) fn is_eagain(err: &ffmpeg::Error) -> bool {
    matches!(
        err,
        ffmpeg::Error::Other { errno } if *errno == ffmpeg::error::EAGAIN
    )
}

/// Copy an RGBA buffer into an FFmpeg video frame, respecting stride.
pub(crate) fn copy_rgba_into_frame(frame: &mut ffmpeg::frame::Video, width: u32, rgba: &[u8]) {
    let stride = frame.stride(0);
    let row_bytes = width as usize * 4;
    let height = frame.height() as usize;
    let dst = frame.data_mut(0);

    for y in 0..height {
        let src_start = y * row_bytes;
        let src_end = src_start + row_bytes;
        let dst_start = y * stride;
        let dst_end = dst_start + row_bytes;
        dst[dst_start..dst_end].copy_from_slice(&rgba[src_start..src_end]);
    }
}
