use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RectPatch {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FrameBlockKind {
    Keyframe { rgba: Vec<u8> },
    Delta { patches: Vec<RectPatch> },
    Duplicate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameBlock {
    pub timestamp_ms: u64,
    pub duration_ms: u32,
    pub width: u32,
    pub height: u32,
    pub kind: FrameBlockKind,
}

pub struct FrameCacheWriter {
    writer: BufWriter<File>,
}

impl FrameCacheWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    pub fn write_block(&mut self, block: &FrameBlock) -> Result<()> {
        let serialized = bincode::serialize(block)
            .map_err(|e| ScreenRecorderError::Io(std::io::Error::other(e)))?;
        let compressed = zstd::stream::encode_all(&serialized[..], 3)?;
        let len = compressed.len() as u32;
        self.writer.write_all(&len.to_le_bytes())?;
        self.writer.write_all(&compressed)?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ReconstructedFrame {
    pub timestamp_ms: u64,
    pub duration_ms: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn read_blocks(path: &Path) -> Result<Vec<FrameBlock>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut blocks = Vec::new();

    loop {
        let mut len_bytes = [0u8; 4];
        match reader.read_exact(&mut len_bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(ScreenRecorderError::Io(e)),
        }

        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut compressed = vec![0u8; len];
        reader.read_exact(&mut compressed)?;
        let decompressed = zstd::stream::decode_all(&compressed[..])?;
        let block: FrameBlock = bincode::deserialize(&decompressed)
            .map_err(|e| ScreenRecorderError::Decode(format!("invalid frame cache block: {e}")))?;
        blocks.push(block);
    }

    Ok(blocks)
}

pub fn reconstruct_frames(path: &Path) -> Result<Vec<ReconstructedFrame>> {
    let blocks = read_blocks(path)?;
    let mut out = Vec::new();
    let mut prev: Option<ReconstructedFrame> = None;

    for block in blocks {
        let frame = match block.kind {
            FrameBlockKind::Keyframe { rgba } => ReconstructedFrame {
                timestamp_ms: block.timestamp_ms,
                duration_ms: block.duration_ms,
                width: block.width,
                height: block.height,
                rgba,
            },
            FrameBlockKind::Delta { patches } => {
                let mut base = prev.as_ref().map(|f| f.rgba.clone()).ok_or_else(|| {
                    ScreenRecorderError::Decode("delta block without previous frame".to_string())
                })?;
                apply_patches(&mut base, block.width, block.height, &patches)?;
                ReconstructedFrame {
                    timestamp_ms: block.timestamp_ms,
                    duration_ms: block.duration_ms,
                    width: block.width,
                    height: block.height,
                    rgba: base,
                }
            }
            FrameBlockKind::Duplicate => {
                let mut copy = prev.clone().ok_or_else(|| {
                    ScreenRecorderError::Decode(
                        "duplicate block without previous frame".to_string(),
                    )
                })?;
                copy.timestamp_ms = block.timestamp_ms;
                copy.duration_ms = block.duration_ms;
                copy
            }
        };

        if frame.rgba.len() != (frame.width as usize * frame.height as usize * 4) {
            return Err(ScreenRecorderError::Decode(
                "frame cache block has invalid rgba size".to_string(),
            ));
        }

        prev = Some(frame.clone());
        out.push(frame);
    }

    Ok(out)
}

pub fn apply_patches(
    base: &mut [u8],
    width: u32,
    height: u32,
    patches: &[RectPatch],
) -> Result<()> {
    let stride = width as usize * 4;
    let max_len = width as usize * height as usize * 4;
    if base.len() != max_len {
        return Err(ScreenRecorderError::Decode(
            "patch base frame has invalid size".to_string(),
        ));
    }

    for patch in patches {
        if patch.x >= width || patch.y >= height {
            continue;
        }

        let pw = patch.width.min(width - patch.x) as usize;
        let ph = patch.height.min(height - patch.y) as usize;
        let expected_len = pw
            .checked_mul(ph)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| ScreenRecorderError::Decode("patch overflow".to_string()))?;

        if patch.rgba.len() != expected_len {
            return Err(ScreenRecorderError::Decode(
                "patch rgba size mismatch".to_string(),
            ));
        }

        for row in 0..ph {
            let src_start = row * pw * 4;
            let src_end = src_start + pw * 4;

            let dst_row = patch.y as usize + row;
            let dst_col = patch.x as usize;
            let dst_start = dst_row * stride + dst_col * 4;
            let dst_end = dst_start + pw * 4;

            base[dst_start..dst_end].copy_from_slice(&patch.rgba[src_start..src_end]);
        }
    }

    Ok(())
}

pub fn extract_patch(
    frame_rgba: &[u8],
    frame_width: u32,
    frame_height: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<RectPatch> {
    let fw = frame_width as usize;
    let fh = frame_height as usize;
    if frame_rgba.len() != fw * fh * 4 {
        return Err(ScreenRecorderError::Encode(
            "extract_patch input frame has invalid size".to_string(),
        ));
    }

    let x = x.min(frame_width);
    let y = y.min(frame_height);
    let width = width.min(frame_width.saturating_sub(x));
    let height = height.min(frame_height.saturating_sub(y));

    let pw = width as usize;
    let ph = height as usize;
    let mut rgba = vec![0u8; pw * ph * 4];
    let frame_stride = fw * 4;

    for row in 0..ph {
        let src_start = (y as usize + row) * frame_stride + x as usize * 4;
        let src_end = src_start + pw * 4;

        let dst_start = row * pw * 4;
        let dst_end = dst_start + pw * 4;
        rgba[dst_start..dst_end].copy_from_slice(&frame_rgba[src_start..src_end]);
    }

    Ok(RectPatch {
        x,
        y,
        width,
        height,
        rgba,
    })
}
