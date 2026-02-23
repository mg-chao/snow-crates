use std::fs;
use std::path::PathBuf;

use crate::config::{RecordingAudioFormat, RecordingConfig};
use crate::error::Result;

#[derive(Clone, Debug)]
pub struct TempLayout {
    pub output_dir: PathBuf,
    pub root_tmp_dir: PathBuf,
    pub session_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub video_temp_path: PathBuf,
    pub frame_cache_path: PathBuf,
    pub audio_system_path: PathBuf,
    pub audio_mic_path: PathBuf,
    pub mouse_path: PathBuf,
}

impl TempLayout {
    pub fn create(config: &RecordingConfig, session_id: &str) -> Result<Self> {
        fs::create_dir_all(&config.output_dir)?;
        let root_tmp_dir = config.output_dir.join(".snowtmp");
        fs::create_dir_all(&root_tmp_dir)?;

        let session_dir = root_tmp_dir.join(session_id);
        fs::create_dir_all(&session_dir)?;
        let audio_ext = match config.audio.format {
            RecordingAudioFormat::Mp3 => "mp3",
            RecordingAudioFormat::Aac => "aac",
        };

        let layout = Self {
            output_dir: config.output_dir.clone(),
            root_tmp_dir,
            session_dir: session_dir.clone(),
            manifest_path: session_dir.join("manifest.json"),
            video_temp_path: session_dir.join("video.mp4"),
            frame_cache_path: session_dir.join("frames.srf"),
            audio_system_path: session_dir.join(format!("audio_system.{audio_ext}")),
            audio_mic_path: session_dir.join(format!("audio_mic.{audio_ext}")),
            mouse_path: session_dir.join("mouse.bin"),
        };

        Ok(layout)
    }
}
