use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Identifies a monitor by its stable ID string.
///
/// The stable ID has the format `"{adapter_luid:016x}-{output_id:016x}"`,
/// matching the format produced by `snow_capture::MonitorId::stable_id()`.
/// This decouples the recorder's public API from `snow_capture` internals.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MonitorSelector {
    pub stable_id: String,
}

impl MonitorSelector {
    pub fn new(stable_id: impl Into<String>) -> Self {
        Self {
            stable_id: stable_id.into(),
        }
    }
}

/// Identifies a window by its raw OS handle.
///
/// Wraps the platform-specific window handle (HWND on Windows) without
/// exposing `snow_capture::WindowId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowSelector {
    pub raw_handle: isize,
}

impl WindowSelector {
    pub const fn new(raw_handle: isize) -> Self {
        Self { raw_handle }
    }
}

/// A rectangular region in virtual desktop coordinates.
///
/// Decoupled from `snow_capture::CaptureRegion` so the recorder's
/// public API doesn't depend on capture backend types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordingRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl RecordingRegion {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[derive(Clone, Debug)]
pub enum RecordingTarget {
    PrimaryMonitor,
    Monitor(MonitorSelector),
    Window(WindowSelector),
    Region(RecordingRegion),
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecordingVideoFormat {
    #[default]
    H264Lossless,
    H264,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecordingAudioFormat {
    #[default]
    Mp3,
    Aac,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AudioChannels {
    Mono,
    #[default]
    Stereo,
}

impl AudioChannels {
    pub const fn channels(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum VideoEncodingSpeed {
    UltraFast,
    SuperFast,
    VeryFast,
    Faster,
    Fast,
    #[default]
    Medium,
    Slow,
    Slower,
    VerySlow,
}

impl VideoEncodingSpeed {
    pub const fn as_x264_preset(self) -> &'static str {
        match self {
            Self::UltraFast => "ultrafast",
            Self::SuperFast => "superfast",
            Self::VeryFast => "veryfast",
            Self::Faster => "faster",
            Self::Fast => "fast",
            Self::Medium => "medium",
            Self::Slow => "slow",
            Self::Slower => "slower",
            Self::VerySlow => "veryslow",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoEncodeConfig {
    pub quality: u8,
    pub speed: VideoEncodingSpeed,
}

impl Default for VideoEncodeConfig {
    fn default() -> Self {
        Self {
            quality: 75,
            speed: VideoEncodingSpeed::Medium,
        }
    }
}

impl VideoEncodeConfig {
    pub fn validate(&self, prefix: &str) -> Result<(), String> {
        if self.quality > 100 {
            return Err(format!("{prefix}.quality must be in 0..=100"));
        }

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RecordingAudioConfig {
    pub format: RecordingAudioFormat,
    pub bitrate_kbps: u16,
    pub channels: AudioChannels,
    pub sample_rate_hz: u32,
    pub microphone_enabled: bool,
    pub microphone_device: Option<snow_audio_recorder::DeviceSelector>,
    pub system_audio_enabled: bool,
}

impl Default for RecordingAudioConfig {
    fn default() -> Self {
        Self {
            format: RecordingAudioFormat::Mp3,
            bitrate_kbps: 192,
            channels: AudioChannels::Stereo,
            sample_rate_hz: 48_000,
            microphone_enabled: true,
            microphone_device: None,
            system_audio_enabled: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecordingConfig {
    pub target: RecordingTarget,
    pub output_dir: PathBuf,
    pub keep_temp_files: bool,
    pub fps: u32,
    pub video_format: RecordingVideoFormat,
    pub video: VideoEncodeConfig,
    pub audio: RecordingAudioConfig,
}

impl RecordingConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.fps == 0 {
            return Err("fps must be > 0".to_string());
        }

        if self.output_dir.as_os_str().is_empty() {
            return Err("output_dir must not be empty".to_string());
        }

        self.video.validate("video")?;

        if self.audio.bitrate_kbps == 0 {
            return Err("audio bitrate must be > 0".to_string());
        }

        if self.audio.sample_rate_hz == 0 {
            return Err("audio sample rate must be > 0".to_string());
        }

        if !self.audio.microphone_enabled && !self.audio.system_audio_enabled {
            return Ok(());
        }

        Ok(())
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            target: RecordingTarget::PrimaryMonitor,
            output_dir: PathBuf::from("."),
            keep_temp_files: false,
            fps: 60,
            video_format: RecordingVideoFormat::H264Lossless,
            video: VideoEncodeConfig::default(),
            audio: RecordingAudioConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct AudioEditConfig {
    pub enabled: bool,
    pub volume: f32,
}

impl Default for AudioEditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MouseEditConfig {
    pub visible: bool,
    pub trail_enabled: bool,
    pub trail_window_ms: u64,
    pub trail_smooth_step_px: f32,
    pub trail_max_alpha: u8,
    pub trail_color: [u8; 3],
    pub trail_thickness: i32,
    pub click_enabled: bool,
}

impl Default for MouseEditConfig {
    fn default() -> Self {
        Self {
            visible: true,
            trail_enabled: false,
            trail_window_ms: 128,
            trail_smooth_step_px: 2.0,
            trail_max_alpha: 180,
            trail_color: [255, 32, 32],
            trail_thickness: 2,
            click_enabled: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExportFormat {
    #[default]
    Mp4,
    Avi,
    Gif,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExportConfig {
    pub format: ExportFormat,
    pub output_path: PathBuf,
    pub video: VideoEncodeConfig,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            format: ExportFormat::Mp4,
            output_path: PathBuf::from("output.mp4"),
            video: VideoEncodeConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EditConfig {
    pub playback_speed: f32,
    pub system_audio: AudioEditConfig,
    pub microphone_audio: AudioEditConfig,
    pub mouse: MouseEditConfig,
    pub export: ExportConfig,
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            playback_speed: 1.0,
            system_audio: AudioEditConfig::default(),
            microphone_audio: AudioEditConfig::default(),
            mouse: MouseEditConfig::default(),
            export: ExportConfig::default(),
        }
    }
}

impl EditConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(0.25..=4.0).contains(&self.playback_speed) {
            return Err("playback_speed must be in 0.25..=4.0".to_string());
        }

        if !(0.0..=2.0).contains(&self.system_audio.volume) {
            return Err("system_audio.volume must be in 0.0..=2.0".to_string());
        }

        if !(0.0..=2.0).contains(&self.microphone_audio.volume) {
            return Err("microphone_audio.volume must be in 0.0..=2.0".to_string());
        }

        if self.mouse.trail_window_ms == 0 {
            return Err("mouse.trail_window_ms must be > 0".to_string());
        }

        if !self.mouse.trail_smooth_step_px.is_finite() || self.mouse.trail_smooth_step_px <= 0.0 {
            return Err("mouse.trail_smooth_step_px must be finite and > 0".to_string());
        }

        if self.mouse.trail_thickness < 0 {
            return Err("mouse.trail_thickness must be >= 0".to_string());
        }

        if self.export.output_path.as_os_str().is_empty() {
            return Err("export.output_path must not be empty".to_string());
        }

        self.export.video.validate("export.video")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_config_validation_checks_ranges() {
        let mut cfg = EditConfig::default();
        cfg.playback_speed = 0.1;
        assert!(cfg.validate().is_err());

        cfg = EditConfig::default();
        cfg.system_audio.volume = 2.5;
        assert!(cfg.validate().is_err());

        cfg = EditConfig::default();
        cfg.microphone_audio.volume = 2.5;
        assert!(cfg.validate().is_err());

        cfg = EditConfig::default();
        cfg.mouse.trail_window_ms = 0;
        assert!(cfg.validate().is_err());

        cfg = EditConfig::default();
        cfg.mouse.trail_smooth_step_px = 0.0;
        assert!(cfg.validate().is_err());

        cfg = EditConfig::default();
        cfg.mouse.trail_thickness = -1;
        assert!(cfg.validate().is_err());

        cfg = EditConfig::default();
        cfg.export.video.quality = 101;
        assert!(cfg.validate().is_err());
    }
}
