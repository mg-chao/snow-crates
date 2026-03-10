use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::config::ExportFormat;
use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Debug)]
pub struct ExportResult {
    pub output_path: PathBuf,
    pub duration_ms: u64,
    pub format: ExportFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportStage {
    Analyze,
    VideoDecode,
    VideoProcess,
    VideoEncode,
    AudioMix,
    AudioEncode,
    Mux,
    Finalize,
}

#[derive(Clone, Debug)]
pub struct ExportProgress {
    pub stage: ExportStage,
    pub percent: f32,
    pub video_fps: f32,
    pub eta_ms: Option<u64>,
}

pub struct ExportTask {
    cancel_flag: Arc<AtomicBool>,
    progress_rx: crossbeam_channel::Receiver<ExportProgress>,
    join: Option<JoinHandle<Result<ExportResult>>>,
}

impl ExportTask {
    pub(crate) fn new(
        cancel_flag: Arc<AtomicBool>,
        progress_rx: crossbeam_channel::Receiver<ExportProgress>,
        join: JoinHandle<Result<ExportResult>>,
    ) -> Self {
        Self {
            cancel_flag,
            progress_rx,
            join: Some(join),
        }
    }

    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Release);
    }

    pub fn progress(&self) -> crossbeam_channel::Receiver<ExportProgress> {
        self.progress_rx.clone()
    }

    pub fn wait(mut self) -> Result<ExportResult> {
        let handle = self.join.take().ok_or_else(|| {
            ScreenRecorderError::Export("export task has already been awaited".to_string())
        })?;
        handle
            .join()
            .map_err(|_| ScreenRecorderError::Export("export task panicked".to_string()))?
    }
}
