use std::path::PathBuf;

use crate::config::ExportFormat;

#[derive(Clone, Debug)]
pub struct ExportResult {
    pub output_path: PathBuf,
    pub duration_ms: u64,
    pub format: ExportFormat,
}
