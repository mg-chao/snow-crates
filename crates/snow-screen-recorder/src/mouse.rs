use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CursorShapeCompositionMode {
    AlphaBlend,
    MaskedColor,
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
pub struct CursorShapeModeRecord {
    pub shape_id: u32,
    pub mode: CursorShapeCompositionMode,
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
    CursorShapeMode(CursorShapeModeRecord),
}

pub fn write_mouse_records(path: &Path, records: &[MouseRecord]) -> Result<()> {
    let file = File::create(path)?;
    bincode::serialize_into(BufWriter::new(file), records)
        .map_err(|err| ScreenRecorderError::Io(std::io::Error::other(err)))
}

pub fn read_mouse_records(path: &Path) -> Result<Vec<MouseRecord>> {
    let file = File::open(path)?;
    bincode::deserialize_from(BufReader::new(file))
        .map_err(|err| ScreenRecorderError::Decode(format!("failed to decode mouse store: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_store_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "snow-screen-recorder-mouse-store-{}.bin",
            uuid::Uuid::new_v4().simple()
        ));

        let records = vec![MouseRecord::CursorSample(CursorSampleRecord {
            timestamp_ms: 10,
            x: 20,
            y: 30,
            visible: true,
            shape_id: None,
        })];

        write_mouse_records(&path, &records).expect("write should succeed");
        let decoded = read_mouse_records(&path).expect("read should succeed");
        assert_eq!(decoded.len(), 1);

        let _ = std::fs::remove_file(path);
    }
}
