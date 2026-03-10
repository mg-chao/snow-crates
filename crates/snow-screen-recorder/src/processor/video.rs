use std::path::PathBuf;

use crate::config::{RecordingVideoFormat, VideoEncodeConfig};
use crate::error::{Result, ScreenRecorderError};
use crate::recording::LiveVideoEncoder;

/// Processes video frames: encoding to H.264 via ffmpeg.
///
/// Lazily creates a `LiveVideoEncoder` on the first real frame and
/// stores the last encoded RGBA buffer for tail-frame finalization.
pub(crate) struct VideoProcessor {
    encoder: Option<LiveVideoEncoder>,
    width: u32,
    height: u32,
    target_fps: u32,
    video_format: RecordingVideoFormat,
    video_config: VideoEncodeConfig,
    last_encoded_rgba: Option<Vec<u8>>,
    video_temp_path: PathBuf,
}

impl VideoProcessor {
    /// Create a new `VideoProcessor` with the given configuration.
    ///
    /// Width and height start at zero and are set on the first frame.
    pub(crate) fn new(
        target_fps: u32,
        video_format: RecordingVideoFormat,
        video_config: VideoEncodeConfig,
        video_temp_path: PathBuf,
    ) -> Self {
        Self {
            encoder: None,
            width: 0,
            height: 0,
            target_fps,
            video_format,
            video_config,
            last_encoded_rgba: None,
            video_temp_path,
        }
    }

    /// Validate and lock the resolution on the first frame, or return
    /// an error if a subsequent frame has a different resolution.
    pub(crate) fn handle_resolution_change(&mut self, width: u32, height: u32) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            self.width = width;
            self.height = height;
        } else if self.width != width || self.height != height {
            return Err(ScreenRecorderError::Encode(format!(
                "dynamic resolution is unsupported (expected {}x{}, got {}x{})",
                self.width, self.height, width, height
            )));
        }
        Ok(())
    }

    /// Lazily initialize the video encoder and return a mutable handle.
    fn ensure_encoder(&mut self, width: u32, height: u32) -> Result<&mut LiveVideoEncoder> {
        if self.encoder.is_none() {
            self.encoder = Some(LiveVideoEncoder::create(
                &self.video_temp_path,
                width,
                height,
                self.target_fps,
                self.video_format,
                &self.video_config,
            )?);
        }

        self.encoder.as_mut().ok_or_else(|| {
            ScreenRecorderError::Encode(
                "video encoder was not initialized before frame encoding".to_string(),
            )
        })
    }

    /// Encode a non-duplicate RGBA frame at the given timestamp.
    ///
    /// Lazily creates the encoder on the first real frame.
    /// After encoding, the RGBA buffer is stored as `last_encoded_rgba`
    /// so it can be used as a tail frame during finalization.
    pub(crate) fn encode_frame(
        &mut self,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        ts_ms: u64,
    ) -> Result<()> {
        let encoder = self.ensure_encoder(width, height)?;
        encoder.encode_frame(&rgba, ts_ms)?;
        self.last_encoded_rgba = Some(rgba);
        Ok(())
    }

    /// Return the current resolved width.
    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    /// Return the current resolved height.
    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// Return a reference to the last encoded RGBA buffer, if any.
    pub(crate) fn last_encoded_rgba(&self) -> Option<&[u8]> {
        self.last_encoded_rgba.as_deref()
    }

    /// Take ownership of the encoder for finalization.
    pub(crate) fn take_encoder(&mut self) -> Option<LiveVideoEncoder> {
        self.encoder.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RecordingVideoFormat, VideoEncodeConfig};
    use std::path::PathBuf;

    fn make_processor() -> VideoProcessor {
        VideoProcessor::new(
            30,
            RecordingVideoFormat::H264Lossless,
            VideoEncodeConfig::default(),
            PathBuf::from("/tmp/test-video.h264"),
        )
    }

    #[test]
    fn initial_dimensions_are_zero() {
        let proc = make_processor();
        assert_eq!(proc.width(), 0);
        assert_eq!(proc.height(), 0);
    }

    #[test]
    fn first_resolution_change_sets_dimensions() {
        let mut proc = make_processor();
        proc.handle_resolution_change(1920, 1080).unwrap();
        assert_eq!(proc.width(), 1920);
        assert_eq!(proc.height(), 1080);
    }

    #[test]
    fn same_resolution_is_accepted() {
        let mut proc = make_processor();
        proc.handle_resolution_change(1920, 1080).unwrap();
        proc.handle_resolution_change(1920, 1080).unwrap();
        assert_eq!(proc.width(), 1920);
        assert_eq!(proc.height(), 1080);
    }

    #[test]
    fn different_resolution_is_rejected() {
        let mut proc = make_processor();
        proc.handle_resolution_change(1920, 1080).unwrap();
        let result = proc.handle_resolution_change(1280, 720);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("dynamic resolution is unsupported"));
    }

    #[test]
    fn last_encoded_rgba_is_none_initially() {
        let proc = make_processor();
        assert!(proc.last_encoded_rgba().is_none());
    }

    #[test]
    fn take_encoder_is_none_initially() {
        let mut proc = make_processor();
        assert!(proc.take_encoder().is_none());
    }
}
