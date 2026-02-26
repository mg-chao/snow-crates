use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, ScreenRecorderError};

pub const MOUSE_STORE_SCHEMA_VERSION: u16 = 2;

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

impl From<snow_cursor_capture::CursorCompositionMode> for CursorShapeCompositionMode {
    fn from(mode: snow_cursor_capture::CursorCompositionMode) -> Self {
        match mode {
            snow_cursor_capture::CursorCompositionMode::AlphaBlend => Self::AlphaBlend,
            snow_cursor_capture::CursorCompositionMode::MaskedColor => Self::MaskedColor,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CursorShapeRecord {
    pub shape_id: u64,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub width: u32,
    pub height: u32,
    pub mode: CursorShapeCompositionMode,
    pub shape_rgba: Vec<u8>,
}

impl CursorShapeRecord {
    /// Convert from the canonical cursor-capture type.
    pub fn from_cursor_shape(shape: &snow_cursor_capture::CursorShape) -> Self {
        Self {
            shape_id: shape.shape_id,
            hotspot_x: shape.hotspot_x,
            hotspot_y: shape.hotspot_y,
            width: shape.width,
            height: shape.height,
            mode: shape.composition_mode.into(),
            shape_rgba: shape.shape_rgba.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CursorFrameRecord {
    pub timestamp_ms: u64,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape_id: Option<u64>,
}

impl CursorFrameRecord {
    /// Convert from the canonical cursor-capture type with timestamp
    /// and coordinate translation.
    pub fn from_sample(
        sample: &snow_cursor_capture::CursorFrameSample,
        timestamp_ms: u64,
        origin_x: i32,
        origin_y: i32,
    ) -> Self {
        Self {
            timestamp_ms,
            x: sample.position_x - origin_x,
            y: sample.position_y - origin_y,
            visible: sample.visible,
            shape_id: sample.shape_id,
        }
    }
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
pub struct MouseStore {
    pub schema_version: u16,
    pub cursor_shapes: Vec<CursorShapeRecord>,
    pub cursor_frames: Vec<CursorFrameRecord>,
    pub clicks: Vec<ClickEventRecord>,
}

impl MouseStore {
    pub fn new() -> Self {
        Self {
            schema_version: MOUSE_STORE_SCHEMA_VERSION,
            cursor_shapes: Vec::new(),
            cursor_frames: Vec::new(),
            clicks: Vec::new(),
        }
    }
}

pub fn write_mouse_records(path: &Path, store: &MouseStore) -> Result<()> {
    let file = File::create(path)?;
    bincode::serialize_into(BufWriter::new(file), store)
        .map_err(|err| ScreenRecorderError::Io(std::io::Error::other(err)))
}

pub fn read_mouse_records(path: &Path) -> Result<MouseStore> {
    let file = File::open(path)?;
    let store: MouseStore = bincode::deserialize_from(BufReader::new(file)).map_err(|err| {
        ScreenRecorderError::Decode(format!("failed to decode mouse store: {err}"))
    })?;

    if store.schema_version != MOUSE_STORE_SCHEMA_VERSION {
        return Err(ScreenRecorderError::Decode(format!(
            "unsupported mouse store schema version {}, expected {}",
            store.schema_version, MOUSE_STORE_SCHEMA_VERSION
        )));
    }

    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn mouse_store_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "snow-screen-recorder-mouse-store-{}.bin",
            uuid::Uuid::new_v4().simple()
        ));

        let mut store = MouseStore::new();
        store.cursor_frames.push(CursorFrameRecord {
            timestamp_ms: 10,
            x: 20,
            y: 30,
            visible: true,
            shape_id: None,
        });

        write_mouse_records(&path, &store).expect("write should succeed");
        let decoded = read_mouse_records(&path).expect("read should succeed");
        assert_eq!(decoded.cursor_frames.len(), 1);

        let _ = std::fs::remove_file(path);
    }

    fn arb_composition_mode() -> impl Strategy<Value = snow_cursor_capture::CursorCompositionMode> {
        prop_oneof![
            Just(snow_cursor_capture::CursorCompositionMode::AlphaBlend),
            Just(snow_cursor_capture::CursorCompositionMode::MaskedColor),
        ]
    }

    fn arb_cursor_shape() -> impl Strategy<Value = snow_cursor_capture::CursorShape> {
        (
            any::<u64>(),
            any::<u32>(),
            any::<u32>(),
            1u32..64,
            1u32..64,
            arb_composition_mode(),
            proptest::collection::vec(any::<u8>(), 0..256),
        )
            .prop_map(
                |(sid, hx, hy, w, h, mode, rgba)| snow_cursor_capture::CursorShape {
                    shape_id: sid,
                    hotspot_x: hx,
                    hotspot_y: hy,
                    width: w,
                    height: h,
                    composition_mode: mode,
                    shape_rgba: rgba,
                },
            )
    }

    fn arb_cursor_frame_sample() -> impl Strategy<Value = snow_cursor_capture::CursorFrameSample> {
        (
            -10_000i32..10_000,
            -10_000i32..10_000,
            any::<bool>(),
            proptest::option::of(any::<u64>()),
        )
            .prop_map(
                |(px, py, vis, sid)| snow_cursor_capture::CursorFrameSample {
                    position_x: px,
                    position_y: py,
                    visible: vis,
                    shape_id: sid,
                    shape: None,
                },
            )
    }

    proptest! {
        /// From conversion on CursorCompositionMode preserves semantic
        /// equivalence: AlphaBlend ↔ AlphaBlend, MaskedColor ↔ MaskedColor.
        #[test]
        fn prop_composition_mode_from_preserves_semantics(
            mode in arb_composition_mode(),
        ) {
            let converted: CursorShapeCompositionMode = mode.into();
            match mode {
                snow_cursor_capture::CursorCompositionMode::AlphaBlend => {
                    prop_assert_eq!(converted, CursorShapeCompositionMode::AlphaBlend);
                }
                snow_cursor_capture::CursorCompositionMode::MaskedColor => {
                    prop_assert_eq!(converted, CursorShapeCompositionMode::MaskedColor);
                }
            }
        }

        /// `from_cursor_shape` produces a `CursorShapeRecord` whose every
        /// field matches the source `CursorShape`.
        #[test]
        fn prop_from_cursor_shape_preserves_all_fields(
            shape in arb_cursor_shape(),
        ) {
            let record = CursorShapeRecord::from_cursor_shape(&shape);

            prop_assert_eq!(record.shape_id, shape.shape_id);
            prop_assert_eq!(record.hotspot_x, shape.hotspot_x);
            prop_assert_eq!(record.hotspot_y, shape.hotspot_y);
            prop_assert_eq!(record.width, shape.width);
            prop_assert_eq!(record.height, shape.height);
            prop_assert_eq!(record.shape_rgba, shape.shape_rgba);

            let expected_mode: CursorShapeCompositionMode = shape.composition_mode.into();
            prop_assert_eq!(record.mode, expected_mode);
        }

        /// `from_sample` translates coordinates by subtracting origin and
        /// preserves all other fields from the source `CursorFrameSample`.
        #[test]
        fn prop_from_sample_translates_coordinates(
            sample in arb_cursor_frame_sample(),
            timestamp_ms in any::<u64>(),
            origin_x in -10_000i32..10_000,
            origin_y in -10_000i32..10_000,
        ) {
            let record = CursorFrameRecord::from_sample(
                &sample, timestamp_ms, origin_x, origin_y,
            );

            prop_assert_eq!(record.timestamp_ms, timestamp_ms);
            prop_assert_eq!(record.x, sample.position_x - origin_x);
            prop_assert_eq!(record.y, sample.position_y - origin_y);
            prop_assert_eq!(record.visible, sample.visible);
            prop_assert_eq!(record.shape_id, sample.shape_id);
        }
    }
}
