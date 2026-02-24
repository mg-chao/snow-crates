#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub(crate) use windows::WindowsCursorSampler as CursorSamplerImpl;

#[cfg(not(target_os = "windows"))]
use crate::CursorCaptureError;
#[cfg(not(target_os = "windows"))]
use crate::CursorProbe;

#[cfg(not(target_os = "windows"))]
pub(crate) struct CursorSamplerImpl;

#[cfg(not(target_os = "windows"))]
impl CursorSamplerImpl {
    pub(crate) fn new() -> Result<Self, CursorCaptureError> {
        Err(CursorCaptureError::UnsupportedPlatform)
    }

    pub(crate) fn sample_cursor(&mut self) -> Result<CursorProbe, CursorCaptureError> {
        Err(CursorCaptureError::UnsupportedPlatform)
    }
}
