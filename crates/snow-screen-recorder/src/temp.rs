use std::fs;
use std::path::PathBuf;

use crate::config::RecordingConfig;
use crate::error::Result;

#[derive(Clone, Debug)]
pub struct TempLayout {
    pub output_dir: PathBuf,
    pub session_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub video_temp_path: PathBuf,
    pub audio_system_path: PathBuf,
    pub audio_mic_path: PathBuf,
    pub mouse_path: PathBuf,
}

impl TempLayout {
    pub fn create(config: &RecordingConfig, session_id: &str) -> Result<Self> {
        fs::create_dir_all(&config.output_dir)?;
        let session_dir = config.output_dir.join(format!(".snowtmp-{session_id}"));
        fs::create_dir_all(&session_dir)?;

        Ok(Self {
            output_dir: config.output_dir.clone(),
            session_dir: session_dir.clone(),
            manifest_path: session_dir.join("manifest.json"),
            // Primary recording video written incrementally during capture.
            video_temp_path: session_dir.join("video_recording.mp4"),
            audio_system_path: session_dir.join("audio_system.pcm"),
            audio_mic_path: session_dir.join("audio_mic.pcm"),
            mouse_path: session_dir.join("mouse.bin"),
        })
    }
}
