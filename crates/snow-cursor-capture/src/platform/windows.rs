use std::mem::{size_of, zeroed};

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CopyIcon, DestroyIcon, GetCursorInfo, GetIconInfo,
};

use crate::{CursorCaptureError, CursorCompositionMode, CursorProbe, ShapePayload};

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
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            return Err(CursorCaptureError::platform("GetCursorInfo failed"));
        }

        let visible = (info.flags.0 & CURSOR_SHOWING.0) != 0;
        let mut probe = CursorProbe {
            position_x: info.ptScreenPos.x,
            position_y: info.ptScreenPos.y,
            visible,
            shape: None,
        };

        if !info.hCursor.is_invalid() {
            probe.shape = extract_shape_payload(info.hCursor);
        }

        Ok(probe)
    }
}

fn extract_shape_payload(
    hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
) -> Option<ShapePayload> {
    let icon = unsafe { CopyIcon(hcursor.into()) }.ok()?;
    if icon.is_invalid() {
        return None;
    }

    let mut icon_info = unsafe { zeroed() };
    let icon_info_ok = unsafe { GetIconInfo(icon, &mut icon_info) }.is_ok();
    if !icon_info_ok {
        let _ = unsafe { DestroyIcon(icon) };
        return None;
    }

    let hotspot_x = icon_info.xHotspot;
    let hotspot_y = icon_info.yHotspot;

    let color_bitmap = icon_info.hbmColor;
    let mask_bitmap = icon_info.hbmMask;

    let shape = if !color_bitmap.is_invalid() {
        extract_color_shape(hotspot_x, hotspot_y, color_bitmap, mask_bitmap)
    } else {
        extract_monochrome_shape(hotspot_x, hotspot_y, mask_bitmap)
    };

    if !color_bitmap.is_invalid() {
        let _ = unsafe { DeleteObject(color_bitmap.into()) };
    }
    if !mask_bitmap.is_invalid() {
        let _ = unsafe { DeleteObject(mask_bitmap.into()) };
    }
    let _ = unsafe { DestroyIcon(icon) };

    shape
}

fn extract_color_shape(
    hotspot_x: u32,
    hotspot_y: u32,
    color: HBITMAP,
    mask: HBITMAP,
) -> Option<ShapePayload> {
    let (width, height, mut rgba) = read_bitmap_rgba(color)?;
    let mut composition_mode = CursorCompositionMode::AlphaBlend;

    if rgba.chunks_exact(4).all(|px| px[3] == 0)
        && !mask.is_invalid()
        && let Some((mask_width, mask_height, mask_rgba)) = read_bitmap_rgba(mask)
        && apply_color_mask_as_masked_composition(
            &mut rgba,
            width,
            height,
            mask_width,
            mask_height,
            &mask_rgba,
        )
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
    let (mask_width, mask_height_full, mask_rgba) = read_bitmap_rgba(mask)?;
    if mask_height_full < 2 {
        return None;
    }
    let height = mask_height_full / 2;
    let width = mask_width;

    let mut rgba = vec![
        0u8;
        (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?
    ];
    for y in 0..height {
        for x in 0..width {
            let and_idx = ((y * width + x) * 4) as usize;
            let xor_idx = (((y + height) * width + x) * 4) as usize;
            let and_set = pixel_is_set(&mask_rgba[and_idx..and_idx + 4]);
            let xor_set = pixel_is_set(&mask_rgba[xor_idx..xor_idx + 4]);

            let (r, g, b, a) = match (and_set, xor_set) {
                (false, false) => (0u8, 0u8, 0u8, 0u8),
                (false, true) => (255u8, 255u8, 255u8, 0u8),
                (true, false) => (0u8, 0u8, 0u8, 255u8),
                (true, true) => (255u8, 255u8, 255u8, 255u8),
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

fn pixel_is_set(pixel: &[u8]) -> bool {
    pixel[0] > 127 || pixel[1] > 127 || pixel[2] > 127
}

fn apply_color_mask_as_masked_composition(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    mask_width: u32,
    mask_height: u32,
    mask_rgba: &[u8],
) -> bool {
    let and_height = if mask_height >= height.saturating_mul(2) {
        height
    } else {
        mask_height.min(height)
    };
    let rows = and_height.min(height);
    let cols = mask_width.min(width);
    if rows == 0 || cols == 0 {
        return false;
    }

    for y in 0..rows {
        for x in 0..cols {
            let idx = ((y * mask_width + x) * 4) as usize;
            if idx + 3 >= mask_rgba.len() {
                return false;
            }
            let mask_set = pixel_is_set(&mask_rgba[idx..idx + 4]);
            let dst = ((y * width + x) * 4 + 3) as usize;
            if dst >= rgba.len() {
                return false;
            }
            // DrawIconEx non-alpha path: dst = (dst & AND) XOR XOR.
            // We encode this into MaskedColor convention:
            // alpha=0x00 => copy XOR color (AND=0), alpha=0xFF => XOR (AND=1).
            rgba[dst] = if mask_set { 255 } else { 0 };
        }
    }

    true
}

fn read_bitmap_rgba(bitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    if bitmap.is_invalid() {
        return None;
    }

    let mut bmp = BITMAP::default();
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

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };

    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.is_invalid() {
        return None;
    }

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
    let _ = unsafe { DeleteDC(hdc) };
    if copied == 0 {
        return None;
    }

    let mut rgba = vec![0u8; bgra.len()];
    for (src, dst) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }

    Some((width, height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_color_mask_as_masked_composition_maps_and_bits_to_alpha_ops() {
        let mut rgba = vec![
            // row 0
            10, 20, 30, 0, 40, 50, 60, 0, // row 1
            70, 80, 90, 0, 100, 110, 120, 0,
        ];
        let mask_rgba = vec![
            // row 0: set, clear
            255, 255, 255, 255, 0, 0, 0, 255, // row 1: clear, set
            0, 0, 0, 255, 255, 255, 255, 255,
        ];

        let ok = apply_color_mask_as_masked_composition(&mut rgba, 2, 2, 2, 2, &mask_rgba);
        assert!(ok, "mask conversion should succeed");

        // AND=1 -> XOR op -> alpha=255, AND=0 -> copy op -> alpha=0.
        assert_eq!(rgba[3], 255);
        assert_eq!(rgba[7], 0);
        assert_eq!(rgba[11], 0);
        assert_eq!(rgba[15], 255);
    }
}
