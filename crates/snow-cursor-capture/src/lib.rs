mod error;
mod platform;

pub use error::CursorCaptureError;

use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum CursorCompositionMode {
    #[default]
    AlphaBlend,
    MaskedColor,
}

#[derive(Clone, Debug)]
pub struct CursorShape {
    pub shape_id: u64,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub width: u32,
    pub height: u32,
    pub composition_mode: CursorCompositionMode,
    pub shape_rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct CursorFrameSample {
    pub position_x: i32,
    pub position_y: i32,
    pub visible: bool,
    pub shape_id: Option<u64>,
    pub shape: Option<CursorShape>,
}

#[derive(Clone, Debug)]
struct ShapePayload {
    hotspot_x: u32,
    hotspot_y: u32,
    width: u32,
    height: u32,
    composition_mode: CursorCompositionMode,
    shape_rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
struct CursorProbe {
    position_x: i32,
    position_y: i32,
    visible: bool,
    shape: Option<ShapePayload>,
}

pub struct CursorSampler {
    inner: platform::CursorSamplerImpl,
    emitted_shape_ids: HashSet<u64>,
    last_shape_id: Option<u64>,
}

impl CursorSampler {
    pub fn new() -> Result<Self, CursorCaptureError> {
        Ok(Self {
            inner: platform::CursorSamplerImpl::new()?,
            emitted_shape_ids: HashSet::new(),
            last_shape_id: None,
        })
    }

    pub fn sample(&mut self) -> Result<CursorFrameSample, CursorCaptureError> {
        let probe = self.inner.sample_cursor()?;
        let mut shape_id = self.last_shape_id;
        let mut shape = None;

        if let Some(payload) = probe.shape {
            let id = hash_shape_payload(&payload);
            shape_id = Some(id);
            self.last_shape_id = Some(id);

            if self.emitted_shape_ids.insert(id) {
                shape = Some(CursorShape {
                    shape_id: id,
                    hotspot_x: payload.hotspot_x,
                    hotspot_y: payload.hotspot_y,
                    width: payload.width,
                    height: payload.height,
                    composition_mode: payload.composition_mode,
                    shape_rgba: payload.shape_rgba,
                });
            }
        }

        Ok(CursorFrameSample {
            position_x: probe.position_x,
            position_y: probe.position_y,
            visible: probe.visible,
            shape_id,
            shape,
        })
    }
}

fn hash_shape_payload(shape: &ShapePayload) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    fn hash_bytes(mut h: u64, bytes: &[u8]) -> u64 {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        h
    }

    let mut h = FNV_OFFSET;
    h = hash_bytes(h, &shape.hotspot_x.to_le_bytes());
    h = hash_bytes(h, &shape.hotspot_y.to_le_bytes());
    h = hash_bytes(h, &shape.width.to_le_bytes());
    h = hash_bytes(h, &shape.height.to_le_bytes());
    h = hash_bytes(
        h,
        &[match shape.composition_mode {
            CursorCompositionMode::AlphaBlend => 0,
            CursorCompositionMode::MaskedColor => 1,
        }],
    );
    hash_bytes(h, &shape.shape_rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(seed: u8) -> ShapePayload {
        ShapePayload {
            hotspot_x: 1,
            hotspot_y: 2,
            width: 2,
            height: 2,
            composition_mode: CursorCompositionMode::AlphaBlend,
            shape_rgba: vec![seed; 16],
        }
    }

    #[test]
    fn shape_id_changes_when_shape_pixels_change() {
        let a = hash_shape_payload(&payload(1));
        let b = hash_shape_payload(&payload(2));
        assert_ne!(a, b);
    }
}
