use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{RecordingVideoFormat, VideoEncodeConfig};
use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PauseInterval {
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionManifest {
    pub session_id: String,
    pub output_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub keep_temp_files: bool,
    pub video_temp_path: PathBuf,
    pub audio_system_path: Option<PathBuf>,
    pub audio_mic_path: Option<PathBuf>,
    pub mouse_path: PathBuf,
    pub fps: u32,
    #[serde(default)]
    pub recording_video_format: RecordingVideoFormat,
    #[serde(default)]
    pub recording_video: VideoEncodeConfig,
    pub width: u32,
    pub height: u32,
    pub capture_origin_x: i32,
    pub capture_origin_y: i32,
    pub recorded_system_audio: bool,
    pub recorded_microphone_audio: bool,
    pub audio_sample_rate_hz: u32,
    pub audio_channels: u16,
    pub audio_bitrate_kbps: u16,
    pub pause_intervals: Vec<PauseInterval>,
}

impl SessionManifest {
    pub fn write_to_path(&self, path: &Path) -> Result<()> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)
            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))
    }

    pub fn read_from_path(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        serde_json::from_reader(file)
            .map_err(|e| ScreenRecorderError::Decode(format!("invalid manifest: {e}")))
    }
}

#[derive(Clone, Debug)]
pub struct RecordingArtifact {
    pub session_id: String,
    pub output_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub recorded_system_audio: bool,
    pub recorded_microphone_audio: bool,
}

impl RecordingArtifact {
    pub fn load_manifest(&self) -> Result<SessionManifest> {
        SessionManifest::read_from_path(&self.manifest_path)
    }
}
