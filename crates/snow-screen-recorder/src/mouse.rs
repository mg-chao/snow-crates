use std::collections::HashMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CallNextHookEx, DispatchMessageW, GetCursorInfo, GetIconInfo,
    HC_ACTION, HHOOK, ICONINFO, MSG, MSLLHOOKSTRUCT, PM_REMOVE, PeekMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_RBUTTONDOWN, WM_RBUTTONUP,
};

use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CursorShapeRecord {
    pub shape_id: u32,
    pub shape_hash: u64,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub width: u32,
    pub height: u32,
    pub shape_rgba: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CursorSampleRecord {
    pub timestamp_ms: u64,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape_id: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClickEventRecord {
    pub timestamp_ms: u64,
    pub x: i32,
    pub y: i32,
    pub button: MouseButton,
    pub down: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MouseRecord {
    CursorShape(CursorShapeRecord),
    CursorSample(CursorSampleRecord),
    Click(ClickEventRecord),
}

#[derive(Clone)]
struct HookRuntime {
    sender: Sender<ClickEventRecord>,
    started_at: Instant,
    capture_origin_x: i32,
    capture_origin_y: i32,
}

fn hook_runtime() -> &'static Mutex<Option<HookRuntime>> {
    static HOOK_RUNTIME: OnceLock<Mutex<Option<HookRuntime>>> = OnceLock::new();
    HOOK_RUNTIME.get_or_init(|| Mutex::new(None))
}

#[derive(Clone)]
struct CursorShapePayload {
    shape_hash: u64,
    hotspot_x: u32,
    hotspot_y: u32,
    width: u32,
    height: u32,
    shape_rgba: Vec<u8>,
}

fn cursor_shape_hash_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    hotspot_x: u32,
    hotspot_y: u32,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    hotspot_x.hash(&mut hasher);
    hotspot_y.hash(&mut hasher);
    rgba.hash(&mut hasher);
    hasher.finish()
}

fn fallback_shape_hash(cursor_handle: usize, hotspot_x: u32, hotspot_y: u32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cursor_handle.hash(&mut hasher);
    hotspot_x.hash(&mut hasher);
    hotspot_y.hash(&mut hasher);
    hasher.finish()
}

fn destroy_icon_info(icon_info: &ICONINFO) {
    if !icon_info.hbmColor.is_invalid() {
        let _ = unsafe { DeleteObject(HGDIOBJ(icon_info.hbmColor.0)) };
    }
    if !icon_info.hbmMask.is_invalid() {
        let _ = unsafe { DeleteObject(HGDIOBJ(icon_info.hbmMask.0)) };
    }
}

fn read_bitmap_bgra(bitmap: HBITMAP) -> Result<(u32, u32, Vec<u8>)> {
    if bitmap.is_invalid() {
        return Err(ScreenRecorderError::Decode(
            "cursor bitmap handle is invalid".to_string(),
        ));
    }

    let mut bmp = BITMAP::default();
    let copied = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bmp as *mut BITMAP).cast()),
        )
    };
    if copied == 0 {
        return Err(ScreenRecorderError::Decode(
            "GetObjectW failed for cursor bitmap".to_string(),
        ));
    }

    let width = bmp.bmWidth.unsigned_abs();
    let height = bmp.bmHeight.unsigned_abs();
    if width == 0 || height == 0 {
        return Err(ScreenRecorderError::Decode(
            "cursor bitmap has invalid dimensions".to_string(),
        ));
    }

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32), // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };

    let mut pixels = vec![
        0u8;
        (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4)
    ];

    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.is_invalid() {
        return Err(ScreenRecorderError::Decode(
            "CreateCompatibleDC failed for cursor extraction".to_string(),
        ));
    }

    let scanlines = unsafe {
        GetDIBits(
            hdc,
            bitmap,
            0,
            height,
            Some(pixels.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    let _ = unsafe { DeleteDC(hdc) };

    if scanlines == 0 {
        return Err(ScreenRecorderError::Decode(
            "GetDIBits failed for cursor extraction".to_string(),
        ));
    }

    Ok((width, height, pixels))
}

fn bgra_to_rgba_in_place(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
}

fn cursor_shape_payload(cursor_info: &CURSORINFO) -> Option<CursorShapePayload> {
    let mut icon_info = ICONINFO::default();
    if unsafe { GetIconInfo(cursor_info.hCursor.into(), &mut icon_info) }.is_err() {
        return None;
    }

    let hotspot_x = icon_info.xHotspot;
    let hotspot_y = icon_info.yHotspot;
    let result = if !icon_info.hbmColor.is_invalid() {
        match read_bitmap_bgra(icon_info.hbmColor) {
            Ok((width, height, mut rgba)) => {
                bgra_to_rgba_in_place(&mut rgba);

                // Some cursor bitmaps carry zero alpha. In that case use the mask.
                if rgba.chunks_exact(4).all(|p| p[3] == 0) && !icon_info.hbmMask.is_invalid() {
                    if let Ok((mask_w, mask_h, mask_bgra)) = read_bitmap_bgra(icon_info.hbmMask) {
                        let rw = width.min(mask_w);
                        let rh = height.min(mask_h);
                        for y in 0..rh as usize {
                            for x in 0..rw as usize {
                                let ridx = (y * width as usize + x) * 4;
                                let midx = (y * mask_w as usize + x) * 4;
                                let and_mask_on = mask_bgra[midx] > 127;
                                rgba[ridx + 3] = if and_mask_on { 0 } else { 255 };
                            }
                        }
                    } else {
                        for px in rgba.chunks_exact_mut(4) {
                            px[3] = 255;
                        }
                    }
                }

                let shape_hash = cursor_shape_hash_rgba(&rgba, width, height, hotspot_x, hotspot_y);
                Some(CursorShapePayload {
                    shape_hash,
                    hotspot_x,
                    hotspot_y,
                    width,
                    height,
                    shape_rgba: rgba,
                })
            }
            Err(_) => None,
        }
    } else if !icon_info.hbmMask.is_invalid() {
        // Monochrome cursor: mask bitmap contains AND/XOR planes stacked vertically.
        match read_bitmap_bgra(icon_info.hbmMask) {
            Ok((width, mask_height, mask_bgra)) => {
                let height = (mask_height / 2).max(1);
                let mut rgba = vec![0u8; width as usize * height as usize * 4];
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let and_idx = (y * width as usize + x) * 4;
                        let xor_y = (y + height as usize).min(mask_height as usize - 1);
                        let xor_idx = (xor_y * width as usize + x) * 4;

                        let and_mask_on = mask_bgra[and_idx] > 127;
                        let xor_mask_on = mask_bgra[xor_idx] > 127;

                        let dst = and_idx;
                        let (r, g, b, a) = if and_mask_on && !xor_mask_on {
                            (0, 0, 0, 0)
                        } else if xor_mask_on {
                            (255, 255, 255, 255)
                        } else {
                            (0, 0, 0, 255)
                        };
                        rgba[dst] = r;
                        rgba[dst + 1] = g;
                        rgba[dst + 2] = b;
                        rgba[dst + 3] = a;
                    }
                }

                let shape_hash = cursor_shape_hash_rgba(&rgba, width, height, hotspot_x, hotspot_y);
                Some(CursorShapePayload {
                    shape_hash,
                    hotspot_x,
                    hotspot_y,
                    width,
                    height,
                    shape_rgba: rgba,
                })
            }
            Err(_) => None,
        }
    } else {
        None
    };

    destroy_icon_info(&icon_info);
    result
}

unsafe extern "system" fn mouse_ll_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && code as u32 == HC_ACTION {
        let (button, down) = match wparam.0 as u32 {
            WM_LBUTTONDOWN => (MouseButton::Left, true),
            WM_LBUTTONUP => (MouseButton::Left, false),
            WM_RBUTTONDOWN => (MouseButton::Right, true),
            WM_RBUTTONUP => (MouseButton::Right, false),
            _ => return unsafe { CallNextHookEx(None, code, wparam, lparam) },
        };

        let ptr = lparam.0 as *const MSLLHOOKSTRUCT;
        if let Some(ms) = unsafe { ptr.as_ref() } {
            if let Ok(guard) = hook_runtime().lock() {
                if let Some(runtime) = guard.as_ref() {
                    let _ = runtime.sender.send(ClickEventRecord {
                        timestamp_ms: runtime.started_at.elapsed().as_millis() as u64,
                        x: ms.pt.x - runtime.capture_origin_x,
                        y: ms.pt.y - runtime.capture_origin_y,
                        button,
                        down,
                    });
                }
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn install_low_level_mouse_hook(
    sender: Sender<ClickEventRecord>,
    started_at: Instant,
    capture_origin_x: i32,
    capture_origin_y: i32,
) -> Result<HHOOK> {
    if let Ok(mut guard) = hook_runtime().lock() {
        *guard = Some(HookRuntime {
            sender,
            started_at,
            capture_origin_x,
            capture_origin_y,
        });
    }

    let hmodule = unsafe { GetModuleHandleW(None) }
        .map_err(|e| ScreenRecorderError::Encode(format!("GetModuleHandleW failed: {e}")))?;

    match unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_ll_hook_proc),
            Some(hmodule.into()),
            0,
        )
    } {
        Ok(hook) => Ok(hook),
        Err(e) => {
            if let Ok(mut guard) = hook_runtime().lock() {
                *guard = None;
            }
            Err(ScreenRecorderError::Encode(format!(
                "SetWindowsHookExW(WH_MOUSE_LL) failed: {e}"
            )))
        }
    }
}

fn uninstall_low_level_mouse_hook(hook: HHOOK) {
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    if let Ok(mut guard) = hook_runtime().lock() {
        *guard = None;
    }
}

fn pump_hook_messages() {
    let mut msg = MSG::default();
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub(crate) fn start_mouse_worker(
    path: std::path::PathBuf,
    stop_flag: Arc<AtomicBool>,
    started_at: Instant,
    capture_origin_x: i32,
    capture_origin_y: i32,
) -> thread::JoinHandle<Result<()>> {
    thread::Builder::new()
        .name("snow-screen-recorder-mouse".to_string())
        .spawn(move || {
            let file = File::create(path)?;
            let mut writer = BufWriter::new(file);

            let (click_tx, click_rx) = crossbeam_channel::unbounded::<ClickEventRecord>();
            let hook = install_low_level_mouse_hook(
                click_tx,
                started_at,
                capture_origin_x,
                capture_origin_y,
            )?;

            let run_result = (|| -> Result<()> {
                let mut shape_ids: HashMap<u64, u32> = HashMap::new();
                let mut next_shape_id: u32 = 1;

                while !stop_flag.load(Ordering::Acquire) {
                    pump_hook_messages();

                    let mut cursor_info = CURSORINFO {
                        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                        ..Default::default()
                    };

                    if unsafe { GetCursorInfo(&mut cursor_info) }.is_ok() {
                        let visible = cursor_info.flags == CURSOR_SHOWING;

                        let shape_payload = if visible {
                            cursor_shape_payload(&cursor_info).unwrap_or_else(|| {
                                let shape_hash =
                                    fallback_shape_hash(cursor_info.hCursor.0 as usize, 0, 0);
                                CursorShapePayload {
                                    shape_hash,
                                    hotspot_x: 0,
                                    hotspot_y: 0,
                                    width: 0,
                                    height: 0,
                                    shape_rgba: Vec::new(),
                                }
                            })
                        } else {
                            CursorShapePayload {
                                shape_hash: 0,
                                hotspot_x: 0,
                                hotspot_y: 0,
                                width: 0,
                                height: 0,
                                shape_rgba: Vec::new(),
                            }
                        };

                        let shape_id = if visible {
                            if let Some(id) = shape_ids.get(&shape_payload.shape_hash).copied() {
                                Some(id)
                            } else {
                                let id = next_shape_id;
                                next_shape_id = next_shape_id.saturating_add(1);
                                shape_ids.insert(shape_payload.shape_hash, id);

                                let shape_record = MouseRecord::CursorShape(CursorShapeRecord {
                                    shape_id: id,
                                    shape_hash: shape_payload.shape_hash,
                                    hotspot_x: shape_payload.hotspot_x,
                                    hotspot_y: shape_payload.hotspot_y,
                                    width: shape_payload.width,
                                    height: shape_payload.height,
                                    shape_rgba: shape_payload.shape_rgba,
                                });
                                bincode::serialize_into(&mut writer, &shape_record).map_err(
                                    |e| ScreenRecorderError::Io(std::io::Error::other(e)),
                                )?;
                                Some(id)
                            }
                        } else {
                            None
                        };

                        let ts = started_at.elapsed().as_millis() as u64;
                        let sample = MouseRecord::CursorSample(CursorSampleRecord {
                            timestamp_ms: ts,
                            x: cursor_info.ptScreenPos.x - capture_origin_x,
                            y: cursor_info.ptScreenPos.y - capture_origin_y,
                            visible,
                            shape_id,
                        });
                        bincode::serialize_into(&mut writer, &sample)
                            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;
                    }

                    while let Ok(click) = click_rx.try_recv() {
                        bincode::serialize_into(&mut writer, &MouseRecord::Click(click))
                            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;
                    }

                    thread::sleep(Duration::from_millis(16));
                }

                // Final drain before shutdown.
                pump_hook_messages();
                while let Ok(click) = click_rx.try_recv() {
                    bincode::serialize_into(&mut writer, &MouseRecord::Click(click))
                        .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;
                }

                writer.flush()?;
                Ok(())
            })();

            uninstall_low_level_mouse_hook(hook);
            run_result
        })
        .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))
        .expect("failed to spawn mouse worker")
}

pub(crate) fn read_mouse_records(path: &std::path::Path) -> Result<Vec<MouseRecord>> {
    let mut file = File::open(path)?;
    let mut out = Vec::new();
    loop {
        match bincode::deserialize_from::<_, MouseRecord>(&mut file) {
            Ok(record) => out.push(record),
            Err(e) => {
                if let bincode::ErrorKind::Io(err) = &*e {
                    if err.kind() == std::io::ErrorKind::UnexpectedEof {
                        break;
                    }
                }
                return Err(ScreenRecorderError::Decode(format!(
                    "failed to decode mouse.bin: {e}"
                )));
            }
        }
    }
    Ok(out)
}
