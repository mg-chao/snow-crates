use std::time::{Duration, Instant};

#[cfg(feature = "cursor")]
pub use snow_cursor_capture::{CursorCompositionMode, CursorFrameSample, CursorShape};

use crate::error::{CaptureError, CaptureResult};
use snow_core::event::StreamEvent;
use snow_core::timestamp::{StreamTimestamp, TickFormat};

/// Color space / transfer function describing the frame's pixel data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorSpace {
    /// Standard sRGB (BT.709 primaries, sRGB transfer function).
    /// This is the default for SDR captures and tonemapped HDR output.
    #[default]
    Srgb,
    /// Scene-referred linear light (BT.709 primaries, linear gamma).
    /// Produced when the DXGI backend captures an HDR surface without
    /// tonemapping.
    LinearSrgb,
    /// HDR10 / PQ (BT.2020 primaries, SMPTE ST 2084 transfer function).
    Hdr10Pq,
    /// Hybrid Log-Gamma (BT.2020 primaries, ARIB STD-B67).
    Hlg,
}

/// Minimum allocation size to attempt large-page backing.
/// 4K RGBA = 3840x2160x4 ~= 33 MB - well above the 2 MB large page size.
/// We only bother for allocations >= 4 MB to avoid overhead on small captures.
const LARGE_PAGE_MIN_BYTES: usize = 4 * 1024 * 1024;

/// A rectangle describing a dirty (changed) region of the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Cursor data captured alongside the frame.
#[cfg(feature = "cursor")]
pub type CursorData = CursorFrameSample;

/// Metadata attached to each captured frame for recording pipelines.
#[derive(Clone, Debug, Default)]
pub struct FrameMetadata {
    /// Wall-clock time spent inside the capture call (GPU readback,
    /// staging copy, pixel conversion). Lets recorders detect when the
    /// capture pipeline itself is the bottleneck vs. the encoder.
    pub capture_duration: Option<Duration>,
    /// Whether this frame's content is identical to the previous frame.
    /// `true` means no new desktop present occurred - a recorder can skip
    /// encoding this frame to save bitrate.
    pub is_duplicate: bool,
    /// Dirty rectangles describing which regions changed since the last
    /// frame. Empty when the backend doesn't support damage tracking or
    /// when the entire frame changed.
    pub dirty_rects: Vec<DirtyRect>,
    /// Cursor shape and position at the time of capture. `None` when
    /// cursor capture is not enabled or not supported by the backend.
    #[cfg(feature = "cursor")]
    pub cursor: Option<CursorData>,
    /// Monotonic sequence number incremented for each capture call.
    /// Useful for correlating frames across threads.
    pub sequence: u64,
    /// Color space / transfer function of the pixel data. Defaults to
    /// `Srgb` for standard dynamic range captures. HDR pipelines can
    /// check this to decide whether tonemapping or passthrough is needed.
    pub color_space: ColorSpace,
    /// Unified timestamp. `tick_format` is `RawQpc`.
    pub stream_timestamp: Option<StreamTimestamp>,
}

impl FrameMetadata {
    /// Set timing fields from a capture operation.
    ///
    /// Populates `stream_timestamp` from the capture time and QPC value.
    pub(crate) fn set_timing(&mut self, capture_time: Option<Instant>, present_time_qpc: Option<i64>) {
        self.stream_timestamp = Some(StreamTimestamp {
            instant: capture_time.unwrap_or_else(Instant::now),
            raw_os_ticks: present_time_qpc,
            tick_format: TickFormat::RawQpc,
        });
    }
}

/// Query the current QPC counter value. Returns `None` on non-Windows
/// or if the call fails.
#[cfg(target_os = "windows")]
pub(crate) fn query_qpc_now() -> Option<i64> {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut ticks = 0i64;
    let ok = unsafe { QueryPerformanceCounter(&mut ticks) };
    if ok.is_ok() { Some(ticks) } else { None }
}

/// Notification sent through the streaming channel when the capture
/// source changes in a way that affects the output.
#[derive(Debug)]
pub enum CaptureEvent {
    /// A new frame is available.
    Frame(Frame),
    /// The capture source resolution changed. A recorder should
    /// reconfigure its encoder with the new dimensions.
    ResolutionChanged {
        old_width: u32,
        old_height: u32,
        new_width: u32,
        new_height: u32,
    },
    /// One or more frames were dropped due to backpressure (the
    /// receiver couldn't keep up). A recorder should insert duplicate
    /// frames to maintain correct A/V timing.
    FrameDropped {
        /// Sequence number of the dropped frame.
        sequence: u64,
    },
    /// The stream was paused. Contains the `Instant` at which the
    /// pause took effect. Recorders can use this to account for the
    /// gap in their muxer timeline.
    Paused { at: Instant },
    /// The stream was resumed after a pause. Contains the `Instant`
    /// of resumption and the total duration of the pause gap.
    Resumed { at: Instant, gap: Duration },
    /// The stream thread is about to exit cleanly (stop was requested).
    /// Sent after the last frame so the consumer knows no more events
    /// will arrive. Useful for flushing encoder queues.
    StreamEnded,
    /// The stream encountered a fatal error and is about to exit.
    /// After receiving this event the channel will disconnect.
    Error(CaptureError),
}

impl StreamEvent for CaptureEvent {
    fn is_paused(&self) -> bool {
        matches!(self, CaptureEvent::Paused { .. })
    }

    fn is_resumed(&self) -> bool {
        matches!(self, CaptureEvent::Resumed { .. })
    }

    fn is_stream_ended(&self) -> bool {
        matches!(self, CaptureEvent::StreamEnded)
    }

    fn is_error(&self) -> bool {
        matches!(self, CaptureEvent::Error(_))
    }

    fn timestamp(&self) -> Option<&StreamTimestamp> {
        match self {
            CaptureEvent::Frame(frame) => frame.metadata.stream_timestamp.as_ref(),
            _ => None,
        }
    }
}

pub struct Frame {
    data: FrameBuffer,
    width: u32,
    height: u32,
    /// Per-frame metadata for recording pipelines.
    pub metadata: FrameMetadata,
}

/// Frame buffer that tries to use large pages (2 MB) via `VirtualAlloc`
/// to reduce TLB misses during parallel pixel conversion.  Falls back
/// to a regular `Vec<u8>` when large pages aren't available or the
/// allocation is too small to benefit.
enum FrameBuffer {
    Vec(Vec<u8>),
    #[cfg(target_os = "windows")]
    LargePage(LargePageAlloc),
}

#[cfg(target_os = "windows")]
struct LargePageAlloc {
    ptr: *mut u8,
    len: usize,
    capacity: usize,
}

#[cfg(target_os = "windows")]
unsafe impl Send for LargePageAlloc {}
#[cfg(target_os = "windows")]
unsafe impl Sync for LargePageAlloc {}

#[cfg(target_os = "windows")]
impl Drop for LargePageAlloc {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            use windows::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
            unsafe {
                let _ = VirtualFree(self.ptr as *mut _, 0, MEM_RELEASE);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn try_alloc_large_pages(size: usize) -> Option<LargePageAlloc> {
    use windows::Win32::System::Memory::{
        GetLargePageMinimum, MEM_COMMIT, MEM_LARGE_PAGES, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };

    if size < LARGE_PAGE_MIN_BYTES {
        return None;
    }

    let large_page_size = unsafe { GetLargePageMinimum() };
    if large_page_size == 0 {
        return None;
    }

    let aligned_size = (size + large_page_size - 1) & !(large_page_size - 1);

    let ptr = unsafe {
        VirtualAlloc(
            None,
            aligned_size,
            MEM_COMMIT | MEM_RESERVE | MEM_LARGE_PAGES,
            PAGE_READWRITE,
        )
    };

    if ptr.is_null() {
        return None;
    }

    Some(LargePageAlloc {
        ptr: ptr as *mut u8,
        len: 0,
        capacity: aligned_size,
    })
}

impl FrameBuffer {
    fn new() -> Self {
        FrameBuffer::Vec(Vec::new())
    }

    fn len(&self) -> usize {
        match self {
            FrameBuffer::Vec(v) => v.len(),
            #[cfg(target_os = "windows")]
            FrameBuffer::LargePage(lp) => lp.len,
        }
    }

    fn capacity(&self) -> usize {
        match self {
            FrameBuffer::Vec(v) => v.capacity(),
            #[cfg(target_os = "windows")]
            FrameBuffer::LargePage(lp) => lp.capacity,
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            FrameBuffer::Vec(v) => v.as_mut_ptr(),
            #[cfg(target_os = "windows")]
            FrameBuffer::LargePage(lp) => lp.ptr,
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            FrameBuffer::Vec(v) => v.as_slice(),
            #[cfg(target_os = "windows")]
            FrameBuffer::LargePage(lp) => unsafe { std::slice::from_raw_parts(lp.ptr, lp.len) },
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            FrameBuffer::Vec(v) => v.as_mut_slice(),
            #[cfg(target_os = "windows")]
            FrameBuffer::LargePage(lp) => unsafe { std::slice::from_raw_parts_mut(lp.ptr, lp.len) },
        }
    }

    /// Ensure the buffer has exactly `len` bytes available.
    /// Tries large pages for big allocations, falls back to Vec.
    fn ensure_len(&mut self, len: usize) {
        if self.len() == len {
            return;
        }

        if len <= self.capacity() {
            match self {
                FrameBuffer::Vec(v) => unsafe { v.set_len(len) },
                #[cfg(target_os = "windows")]
                FrameBuffer::LargePage(lp) => lp.len = len,
            }
            return;
        }

        #[cfg(target_os = "windows")]
        if len >= LARGE_PAGE_MIN_BYTES && let Some(mut lp) = try_alloc_large_pages(len) {
            lp.len = len;
            *self = FrameBuffer::LargePage(lp);
            return;
        }

        let headroom = len / 8;
        let mut v = Vec::with_capacity(len + headroom);
        unsafe { v.set_len(len) };
        *self = FrameBuffer::Vec(v);
    }
}

impl Frame {
    pub fn empty() -> Self {
        Self {
            data: FrameBuffer::new(),
            width: 0,
            height: 0,
            metadata: FrameMetadata::default(),
        }
    }

    pub fn from_rgba8(width: u32, height: u32, data: Vec<u8>) -> CaptureResult<Self> {
        let expected = rgba_len(width, height)?;
        if data.len() != expected {
            return Err(CaptureError::InvalidConfig(format!(
                "RGBA frame data length mismatch: got {}, expected {} for {}x{}",
                data.len(),
                expected,
                width,
                height
            )));
        }

        Ok(Self {
            data: FrameBuffer::Vec(data),
            width,
            height,
            metadata: FrameMetadata::default(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn as_rgba_bytes(&self) -> &[u8] {
        self.data.as_slice()
    }

    pub fn as_mut_rgba_bytes(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    pub(crate) fn as_mut_rgba_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    pub(crate) fn ensure_rgba_capacity(&mut self, width: u32, height: u32) -> CaptureResult<()> {
        let len = rgba_len(width, height)?;
        self.data.ensure_len(len);
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Reset metadata fields to defaults, preserving the pixel buffer.
    /// Called at the start of each capture to avoid stale metadata from
    /// a reused frame leaking into the new result.
    pub(crate) fn reset_metadata(&mut self) {
        self.metadata.stream_timestamp = None;
        self.metadata.capture_duration = None;
        self.metadata.is_duplicate = false;
        self.metadata.dirty_rects.clear();
        #[cfg(feature = "cursor")]
        { self.metadata.cursor = None; }
        self.metadata.color_space = ColorSpace::default();
        // sequence is set by the session, not reset here
    }
}

fn rgba_len(width: u32, height: u32) -> CaptureResult<usize> {
    let w = usize::try_from(width).map_err(|_| CaptureError::BufferOverflow)?;
    let h = usize::try_from(height).map_err(|_| CaptureError::BufferOverflow)?;
    w.checked_mul(h)
        .and_then(|px| px.checked_mul(4))
        .ok_or(CaptureError::BufferOverflow)
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("data_len", &self.data.len())
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use snow_core::timestamp::TickFormat;

    // **Validates: Requirements 1.2, 1.3**
    //
    // Property 2: Tick format matches source type
    //
    // For any `FrameMetadata` with timing set (any combination of
    // `capture_time` and QPC values including `None`),
    // `stream_timestamp.tick_format` SHALL be `TickFormat::RawQpc`.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]
        #[test]
        fn prop_frame_metadata_tick_format_always_rawqpc(
            has_capture_time in proptest::bool::ANY,
            has_qpc in proptest::bool::ANY,
            qpc_value in proptest::num::i64::ANY,
        ) {
            let capture_time = if has_capture_time { Some(Instant::now()) } else { None };
            let qpc = if has_qpc { Some(qpc_value) } else { None };

            let mut meta = FrameMetadata::default();
            meta.set_timing(capture_time, qpc);

            let ts = meta.stream_timestamp.as_ref()
                .expect("stream_timestamp must be Some after set_timing");
            prop_assert_eq!(ts.tick_format, TickFormat::RawQpc,
                "FrameMetadata tick_format must always be RawQpc, got {:?}", ts.tick_format);
        }
    }

    // **Validates: Requirements 1.6, 1.7, 7.2**
    //
    // Property 1: Leaf metadata always populates stream_timestamp
    //
    // For any call to `FrameMetadata::set_timing` with any combination of
    // `capture_time` (Some/None) and QPC values (Some/None), the resulting
    // `stream_timestamp` SHALL be `Some` with a valid `Instant`.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_frame_metadata_always_populates_stream_timestamp(
            has_capture_time in proptest::bool::ANY,
            has_qpc in proptest::bool::ANY,
            qpc_value in proptest::num::i64::ANY,
        ) {
            let capture_time = if has_capture_time { Some(Instant::now()) } else { None };
            let qpc = if has_qpc { Some(qpc_value) } else { None };

            let mut meta = FrameMetadata::default();
            let before = Instant::now();
            meta.set_timing(capture_time, qpc);
            let after = Instant::now();

            // stream_timestamp must always be Some
            let ts = meta.stream_timestamp.as_ref();
            prop_assert!(ts.is_some(), "stream_timestamp must always be Some after set_timing");

            let ts = ts.unwrap();

            // instant must be valid (between before and after, or equal to capture_time)
            if let Some(ct) = capture_time {
                prop_assert_eq!(ts.instant, ct,
                    "instant should equal the provided capture_time");
            } else {
                // When capture_time is None, instant is Instant::now() at call time
                prop_assert!(ts.instant >= before,
                    "instant should be >= time before set_timing call");
                prop_assert!(ts.instant <= after,
                    "instant should be <= time after set_timing call");
            }

            // raw_os_ticks should match the provided QPC
            prop_assert_eq!(ts.raw_os_ticks, qpc,
                "raw_os_ticks should match the provided QPC value");
        }
    }

    // ── Strategies for generating random CaptureEvent variants ──

    /// Build a `Frame` with an optional `stream_timestamp`.
    fn arb_frame(has_timestamp: bool) -> Frame {
        let mut frame = Frame::empty();
        if has_timestamp {
            frame.metadata.set_timing(Some(Instant::now()), Some(42));
        }
        frame
    }

    /// Strategy that produces random `CaptureEvent` variants.
    fn arb_capture_event() -> impl Strategy<Value = CaptureEvent> {
        prop_oneof![
            // Frame with stream_timestamp set
            Just(()).prop_map(|_| CaptureEvent::Frame(arb_frame(true))),
            // Frame without stream_timestamp
            Just(()).prop_map(|_| CaptureEvent::Frame(arb_frame(false))),
            // ResolutionChanged with random dimensions
            (1u32..4096, 1u32..4096, 1u32..4096, 1u32..4096).prop_map(
                |(ow, oh, nw, nh)| CaptureEvent::ResolutionChanged {
                    old_width: ow,
                    old_height: oh,
                    new_width: nw,
                    new_height: nh,
                }
            ),
            // FrameDropped with random sequence
            any::<u64>().prop_map(|seq| CaptureEvent::FrameDropped { sequence: seq }),
            // Paused
            Just(()).prop_map(|_| CaptureEvent::Paused { at: Instant::now() }),
            // Resumed
            (0u64..10_000).prop_map(|gap_ms| CaptureEvent::Resumed {
                at: Instant::now(),
                gap: Duration::from_millis(gap_ms),
            }),
            // StreamEnded
            Just(()).prop_map(|_| CaptureEvent::StreamEnded),
            // Error
            Just(()).prop_map(|_| CaptureEvent::Error(CaptureError::Timeout)),
        ]
    }

    // **Validates: Requirements 3.3, 3.6**
    //
    // Property 3: StreamEvent lifecycle consistency (CaptureEvent)
    //
    // For lifecycle variants (Paused, Resumed, StreamEnded, Error), exactly
    // one lifecycle method returns true. For data/control variants (Frame,
    // FrameDropped, ResolutionChanged), all lifecycle methods return false.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_capture_event_lifecycle_consistency(event in arb_capture_event()) {
            let lifecycle_results = [
                event.is_paused(),
                event.is_resumed(),
                event.is_stream_ended(),
                event.is_error(),
            ];
            let true_count = lifecycle_results.iter().filter(|&&v| v).count();

            let is_lifecycle_variant = matches!(
                event,
                CaptureEvent::Paused { .. }
                    | CaptureEvent::Resumed { .. }
                    | CaptureEvent::StreamEnded
                    | CaptureEvent::Error(_)
            );

            if is_lifecycle_variant {
                // Exactly one lifecycle method returns true
                prop_assert_eq!(
                    true_count, 1,
                    "lifecycle variant should have exactly one true method, got {}",
                    true_count
                );
            } else {
                // Data/control variants: all lifecycle methods return false
                prop_assert_eq!(
                    true_count, 0,
                    "data/control variant should have all false lifecycle methods, got {} true",
                    true_count
                );
            }

            // Verify specific lifecycle method matches the variant
            match &event {
                CaptureEvent::Paused { .. } => {
                    prop_assert!(event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                CaptureEvent::Resumed { .. } => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                CaptureEvent::StreamEnded => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                CaptureEvent::Error(_) => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(event.is_error());
                }
                _ => {}
            }

            // Verify timestamp() behavior:
            // - Frame with stream_timestamp set → Some
            // - All other variants → None
            match &event {
                CaptureEvent::Frame(frame) => {
                    if frame.metadata.stream_timestamp.is_some() {
                        prop_assert!(event.timestamp().is_some(),
                            "Frame with stream_timestamp should return Some from timestamp()");
                    } else {
                        prop_assert!(event.timestamp().is_none(),
                            "Frame without stream_timestamp should return None from timestamp()");
                    }
                }
                _ => {
                    prop_assert!(event.timestamp().is_none(),
                        "Non-Frame variant should return None from timestamp()");
                }
            }
        }
    }
}
