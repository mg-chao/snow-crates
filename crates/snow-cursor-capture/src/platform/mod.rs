#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub(crate) use windows::WindowsCursorSampler as CursorSamplerImpl;

// Stub for non-Windows platforms — immediately returns `UnsupportedPlatform`.
#[cfg(not(target_os = "windows"))]
pub(crate) struct CursorSamplerImpl;

#[cfg(not(target_os = "windows"))]
impl CursorSamplerImpl {
    pub(crate) fn new() -> Result<Self, crate::CursorCaptureError> {
        Err(crate::CursorCaptureError::UnsupportedPlatform)
    }

    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn sample_cursor(&mut self) -> Result<crate::CursorProbe, crate::CursorCaptureError> {
        Err(crate::CursorCaptureError::UnsupportedPlatform)
    }
}
