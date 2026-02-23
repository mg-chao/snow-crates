use std::collections::HashMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetIconInfo, ICONINFO,
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

fn cursor_shape_hash(cursor_handle: usize, hotspot_x: u32, hotspot_y: u32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cursor_handle.hash(&mut hasher);
    hotspot_x.hash(&mut hasher);
    hotspot_y.hash(&mut hasher);
    hasher.finish()
}

fn cursor_hotspot(cursor_info: &CURSORINFO) -> Option<(u32, u32)> {
    let mut icon_info = ICONINFO::default();
    if unsafe { GetIconInfo(cursor_info.hCursor.into(), &mut icon_info) }.is_ok() {
        if !icon_info.hbmColor.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(icon_info.hbmColor.0)) };
        }
        if !icon_info.hbmMask.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(icon_info.hbmMask.0)) };
        }
        Some((icon_info.xHotspot, icon_info.yHotspot))
    } else {
        None
    }
}

fn async_key_down(vk: i32) -> bool {
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
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

            let mut shape_ids: HashMap<u64, u32> = HashMap::new();
            let mut next_shape_id: u32 = 1;
            let mut left_down_prev = false;
            let mut right_down_prev = false;

            while !stop_flag.load(Ordering::Acquire) {
                let mut cursor_info = CURSORINFO {
                    cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                    ..Default::default()
                };

                if unsafe { GetCursorInfo(&mut cursor_info) }.is_ok() {
                    let visible = cursor_info.flags == CURSOR_SHOWING;
                    let (hotspot_x, hotspot_y) = cursor_hotspot(&cursor_info).unwrap_or((0, 0));

                    let shape_id = if visible {
                        let h =
                            cursor_shape_hash(cursor_info.hCursor.0 as usize, hotspot_x, hotspot_y);
                        if let Some(id) = shape_ids.get(&h).copied() {
                            Some(id)
                        } else {
                            let id = next_shape_id;
                            next_shape_id = next_shape_id.saturating_add(1);
                            shape_ids.insert(h, id);

                            let shape_record = MouseRecord::CursorShape(CursorShapeRecord {
                                shape_id: id,
                                shape_hash: h,
                                hotspot_x,
                                hotspot_y,
                                width: 0,
                                height: 0,
                                shape_rgba: Vec::new(),
                            });
                            bincode::serialize_into(&mut writer, &shape_record)
                                .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;
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

                    let left_down = async_key_down(VK_LBUTTON.0 as i32);
                    let right_down = async_key_down(VK_RBUTTON.0 as i32);
                    let x = cursor_info.ptScreenPos.x - capture_origin_x;
                    let y = cursor_info.ptScreenPos.y - capture_origin_y;

                    if left_down != left_down_prev {
                        let click = MouseRecord::Click(ClickEventRecord {
                            timestamp_ms: ts,
                            x,
                            y,
                            button: MouseButton::Left,
                            down: left_down,
                        });
                        bincode::serialize_into(&mut writer, &click)
                            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;
                        left_down_prev = left_down;
                    }

                    if right_down != right_down_prev {
                        let click = MouseRecord::Click(ClickEventRecord {
                            timestamp_ms: ts,
                            x,
                            y,
                            button: MouseButton::Right,
                            down: right_down,
                        });
                        bincode::serialize_into(&mut writer, &click)
                            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;
                        right_down_prev = right_down;
                    }
                }

                thread::sleep(Duration::from_millis(16));
            }

            writer.flush()?;
            Ok(())
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
