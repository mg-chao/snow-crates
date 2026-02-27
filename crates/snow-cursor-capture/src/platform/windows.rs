use std::mem::{size_of, zeroed};

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CopyIcon, DestroyIcon, GetCursorInfo, GetIconInfo, HICON,
};

use crate::{CursorCaptureError, CursorCompositionMode, CursorProbe, ShapePayload};

// ---------------------------------------------------------------------------
// Platform cursor sampler
// ---------------------------------------------------------------------------

pub(crate) struct WindowsCursorSampler;

impl WindowsCursorSampler {
    pub(crate) fn new() -> Result<Self, CursorCaptureError> {
        Ok(Self)
    }

    pub(crate) fn sample_cursor(&mut self) -> Result<CursorProbe, CursorCaptureError> {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };

        // SAFETY: `info.cbSize` is set to the correct struct size; the pointer
        // is valid for the lifetime of this call.
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            return Err(CursorCaptureError::platform("GetCursorInfo failed"));
        }

        let visible = (info.flags.0 & CURSOR_SHOWING.0) != 0;
        let shape = if info.hCursor.is_invalid() {
            None
        } else {
            extract_shape_payload(info.hCursor)
        };

        Ok(CursorProbe {
            position_x: info.ptScreenPos.x,
            position_y: info.ptScreenPos.y,
            visible,
            shape,
        })
    }
}

// ---------------------------------------------------------------------------
// RAII guard for GDI / icon handles
// ---------------------------------------------------------------------------

/// Ensures `DestroyIcon` is called when the guard is dropped.
struct IconGuard(HICON);

impl Drop for IconGuard {
    fn drop(&mut self) {
        // SAFETY: The handle was obtained from `CopyIcon` and is valid.
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

/// Ensures `DeleteObject` is called for an HBITMAP when the guard is dropped.
struct BitmapGuard(HBITMAP);

impl BitmapGuard {
    fn new(h: HBITMAP) -> Option<Self> {
        if h.is_invalid() { None } else { Some(Self(h)) }
    }

    fn handle(&self) -> HBITMAP {
        self.0
    }
}

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        // SAFETY: The handle was obtained from `GetIconInfo` and is valid.
        let _ = unsafe { DeleteObject(self.0.into()) };
    }
}

// ---------------------------------------------------------------------------
// Shape extraction
// ---------------------------------------------------------------------------

fn extract_shape_payload(
    hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
) -> Option<ShapePayload> {
    // SAFETY: `hcursor` is a valid cursor handle from `GetCursorInfo`.
    let icon = unsafe { CopyIcon(hcursor.into()) }.ok()?;
    if icon.is_invalid() {
        return None;
    }
    let _icon_guard = IconGuard(icon);

    // SAFETY: `icon` is a valid icon handle from `CopyIcon`.
    let mut icon_info = unsafe { zeroed() };
    if unsafe { GetIconInfo(icon, &mut icon_info) }.is_err() {
        return None;
    }

    let hotspot_x = icon_info.xHotspot;
    let hotspot_y = icon_info.yHotspot;

    // Wrap bitmaps in RAII guards so they are cleaned up on any exit path.
    let color_guard = BitmapGuard::new(icon_info.hbmColor);
    let mask_guard = BitmapGuard::new(icon_info.hbmMask);

    if let Some(color) = &color_guard {
        let mask_handle = mask_guard.as_ref().map(|g| g.handle());
        extract_color_shape(hotspot_x, hotspot_y, color.handle(), mask_handle)
    } else if let Some(mask) = &mask_guard {
        extract_monochrome_shape(hotspot_x, hotspot_y, mask.handle())
    } else {
        None
    }
}

fn extract_color_shape(
    hotspot_x: u32,
    hotspot_y: u32,
    color: HBITMAP,
    mask: Option<HBITMAP>,
) -> Option<ShapePayload> {
    let (width, height, mut rgba) = read_bitmap_rgba(color)?;
    let mut composition_mode = CursorCompositionMode::AlphaBlend;

    if let Some(mask_bmp) = mask
        && let Some((mw, mh, mask_rgba)) = read_bitmap_rgba(mask_bmp)
        && should_use_masked_color_composition(&rgba, width, height, mw, mh, &mask_rgba)
        && apply_mask_alpha(&mut rgba, width, height, mw, mh, &mask_rgba)
    {
        composition_mode = CursorCompositionMode::MaskedColor;
    }

    Some(ShapePayload {
        hotspot_x,
        hotspot_y,
        width,
        height,
        composition_mode,
        shape_rgba: rgba,
    })
}

fn extract_monochrome_shape(hotspot_x: u32, hotspot_y: u32, mask: HBITMAP) -> Option<ShapePayload> {
    let (width, full_height, mask_rgba) = read_bitmap_rgba(mask)?;
    if full_height < 2 {
        return None;
    }
    let height = full_height / 2;

    let pixel_count = (width as usize).checked_mul(height as usize)?;
    let mut rgba = vec![0u8; pixel_count.checked_mul(4)?];

    for y in 0..height {
        for x in 0..width {
            let and_idx = ((y * width + x) * 4) as usize;
            let xor_idx = (((y + height) * width + x) * 4) as usize;
            let and_set = pixel_is_set(&mask_rgba[and_idx..and_idx + 4]);
            let xor_set = pixel_is_set(&mask_rgba[xor_idx..xor_idx + 4]);

            // Monochrome cursor truth table:
            //   AND=0, XOR=0 → black, transparent
            //   AND=0, XOR=1 → white, transparent
            //   AND=1, XOR=0 → black, opaque
            //   AND=1, XOR=1 → white, opaque (invert)
            let (r, g, b, a) = match (and_set, xor_set) {
                (false, false) => (0, 0, 0, 0),
                (false, true) => (255, 255, 255, 0),
                (true, false) => (0, 0, 0, 255),
                (true, true) => (255, 255, 255, 255),
            };

            let dst = ((y * width + x) * 4) as usize;
            rgba[dst] = r;
            rgba[dst + 1] = g;
            rgba[dst + 2] = b;
            rgba[dst + 3] = a;
        }
    }

    Some(ShapePayload {
        hotspot_x,
        hotspot_y,
        width,
        height,
        composition_mode: CursorCompositionMode::MaskedColor,
        shape_rgba: rgba,
    })
}

// ---------------------------------------------------------------------------
// Mask region helpers (shared geometry computation)
// ---------------------------------------------------------------------------

/// Computes the overlapping rows and columns between a color bitmap and its
/// AND-mask. Returns `None` when the overlap is empty.
fn mask_overlap(width: u32, height: u32, mask_width: u32, mask_height: u32) -> Option<(u32, u32)> {
    let rows = mask_height.min(height);
    let cols = mask_width.min(width);
    if rows == 0 || cols == 0 {
        None
    } else {
        Some((rows, cols))
    }
}

fn checked_rgba_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

fn validated_mask_scan_region(
    width: u32,
    height: u32,
    mask_width: u32,
    mask_height: u32,
    mask_rgba: &[u8],
) -> Option<(u32, u32)> {
    let (rows, cols) = mask_overlap(width, height, mask_width, mask_height)?;
    let mask_len = checked_rgba_len(mask_width, rows)?;
    if mask_rgba.len() < mask_len {
        return None;
    }
    Some((rows, cols))
}

fn pixel_is_set(pixel: &[u8]) -> bool {
    pixel[0] > 127 || pixel[1] > 127 || pixel[2] > 127
}

// ---------------------------------------------------------------------------
// Mask application
// ---------------------------------------------------------------------------

/// Writes AND-mask bits into the alpha channel of `rgba`.
///
/// DrawIconEx non-alpha path: `dst = (dst & AND) XOR XOR`.
/// We encode this into the `MaskedColor` convention:
///   - `alpha = 0x00` → copy XOR color (`AND = 0`)
///   - `alpha = 0xFF` → XOR with screen (`AND = 1`)
fn apply_mask_alpha(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    mask_width: u32,
    mask_height: u32,
    mask_rgba: &[u8],
) -> bool {
    let Some((rows, cols)) =
        validated_mask_scan_region(width, height, mask_width, mask_height, mask_rgba)
    else {
        return false;
    };

    let Some(rgba_len) = checked_rgba_len(width, rows) else {
        return false;
    };
    if rgba.len() < rgba_len {
        return false;
    }

    for y in 0..rows {
        for x in 0..cols {
            let idx = ((y * mask_width + x) * 4) as usize;
            let dst = ((y * width + x) * 4 + 3) as usize;
            rgba[dst] = if pixel_is_set(&mask_rgba[idx..idx + 4]) {
                255
            } else {
                0
            };
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Composition mode heuristics
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct AlphaStats {
    total: usize,
    zero: usize,
    opaque: usize,
    partial: usize,
}

impl AlphaStats {
    fn from_rgba(rgba: &[u8]) -> Self {
        let mut s = Self::default();
        for px in rgba.chunks_exact(4) {
            s.total += 1;
            match px[3] {
                0 => s.zero += 1,
                255 => s.opaque += 1,
                _ => s.partial += 1,
            }
        }
        s
    }
}

fn mask_has_set_bits(
    width: u32,
    height: u32,
    mask_width: u32,
    mask_height: u32,
    mask_rgba: &[u8],
) -> bool {
    let Some((rows, cols)) =
        validated_mask_scan_region(width, height, mask_width, mask_height, mask_rgba)
    else {
        return false;
    };

    for y in 0..rows {
        for x in 0..cols {
            let idx = ((y * mask_width + x) * 4) as usize;
            if pixel_is_set(&mask_rgba[idx..idx + 4]) {
                return true;
            }
        }
    }

    false
}

fn should_use_masked_color_composition(
    rgba: &[u8],
    width: u32,
    height: u32,
    mask_width: u32,
    mask_height: u32,
    mask_rgba: &[u8],
) -> bool {
    let alpha = AlphaStats::from_rgba(rgba);

    // Partial alpha means the cursor already carries per-pixel transparency;
    // no need for mask-based composition.
    if alpha.total == 0 || alpha.partial > 0 {
        return false;
    }

    if !mask_has_set_bits(width, height, mask_width, mask_height, mask_rgba) {
        return false;
    }

    // Legacy color cursor formats often carry unusable alpha (all 0 or all 255)
    // and rely on the AND mask for composition.
    alpha.zero == alpha.total || alpha.opaque == alpha.total
}

// ---------------------------------------------------------------------------
// Bitmap reading
// ---------------------------------------------------------------------------

fn read_bitmap_rgba(bitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    if bitmap.is_invalid() {
        return None;
    }

    let mut bmp = BITMAP::default();
    // SAFETY: `bitmap` is a valid GDI bitmap handle; `bmp` is correctly sized.
    let got = unsafe {
        GetObjectW(
            bitmap.into(),
            size_of::<BITMAP>() as i32,
            Some((&mut bmp as *mut BITMAP).cast()),
        )
    };
    if got <= 0 || bmp.bmWidth <= 0 || bmp.bmHeight == 0 {
        return None;
    }

    let width = bmp.bmWidth as u32;
    let height = bmp.bmHeight.unsigned_abs();
    let pixels = (width as usize).checked_mul(height as usize)?;
    let mut bgra = vec![0u8; pixels.checked_mul(4)?];

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    // SAFETY: We create a temporary compatible DC for the DIB read.
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.is_invalid() {
        return None;
    }

    // SAFETY: `bgra` is large enough for `width * height * 4` bytes.
    let copied = unsafe {
        GetDIBits(
            hdc,
            bitmap,
            0,
            height,
            Some(bgra.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: `hdc` was created by `CreateCompatibleDC` above.
    let _ = unsafe { DeleteDC(hdc) };
    if copied == 0 {
        return None;
    }

    // Convert BGRA → RGBA in-place to avoid a second allocation.
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    Some((width, height, bgra))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_mask_alpha_maps_and_bits_to_alpha_ops() {
        let mut rgba = vec![
            10, 20, 30, 0, 40, 50, 60, 0, // row 1
            70, 80, 90, 0, 100, 110, 120, 0,
        ];
        let mask_rgba = vec![
            255, 255, 255, 255, 0, 0, 0, 255, // row 1: set, clear
            0, 0, 0, 255, 255, 255, 255, 255,
        ];

        let ok = apply_mask_alpha(&mut rgba, 2, 2, 2, 2, &mask_rgba);
        assert!(ok, "mask conversion should succeed");

        assert_eq!(rgba[3], 255);
        assert_eq!(rgba[7], 0);
        assert_eq!(rgba[11], 0);
        assert_eq!(rgba[15], 255);
    }

    #[test]
    fn should_use_masked_color_composition_accepts_all_opaque_alpha_with_mask_bits() {
        let rgba = vec![
            10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let mask_rgba = vec![
            255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255,
        ];

        assert!(should_use_masked_color_composition(
            &rgba, 2, 2, 2, 2, &mask_rgba
        ));
    }

    #[test]
    fn should_use_masked_color_composition_rejects_partial_alpha_even_with_mask() {
        let rgba = vec![
            10, 20, 30, 255, 40, 50, 60, 128, 70, 80, 90, 255, 100, 110, 120, 0,
        ];
        let mask_rgba = vec![
            255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255,
        ];

        assert!(!should_use_masked_color_composition(
            &rgba, 2, 2, 2, 2, &mask_rgba
        ));
    }
}
