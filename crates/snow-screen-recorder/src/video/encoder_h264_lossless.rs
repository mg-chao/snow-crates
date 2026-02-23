use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use less_avc::BitDepth;
use less_avc::nal_unit::InitialNalUnits;
use less_avc::ycbcr_image::{DataPlane, Planes, YCbCrImage};
use less_avc::{H264Writer, LessEncoder};

use crate::error::{Result, ScreenRecorderError};

fn next_multiple(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

#[derive(Clone, Debug)]
struct Yuv420Owned {
    width: u32,
    height: u32,
    y_stride: usize,
    c_stride: usize,
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

impl Yuv420Owned {
    fn as_image(&self) -> YCbCrImage<'_> {
        YCbCrImage {
            planes: Planes::YCbCr((
                DataPlane {
                    data: &self.y,
                    stride: self.y_stride,
                    bit_depth: BitDepth::Depth8,
                },
                DataPlane {
                    data: &self.cb,
                    stride: self.c_stride,
                    bit_depth: BitDepth::Depth8,
                },
                DataPlane {
                    data: &self.cr,
                    stride: self.c_stride,
                    bit_depth: BitDepth::Depth8,
                },
            )),
            width: self.width,
            height: self.height,
        }
    }
}

fn rgb_to_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let rf = r as f32;
    let gf = g as f32;
    let bf = b as f32;

    let y = (0.299 * rf + 0.587 * gf + 0.114 * bf).round();
    let cb = (-0.168736 * rf - 0.331264 * gf + 0.5 * bf + 128.0).round();
    let cr = (0.5 * rf - 0.418688 * gf - 0.081312 * bf + 128.0).round();

    (
        y.clamp(0.0, 255.0) as u8,
        cb.clamp(0.0, 255.0) as u8,
        cr.clamp(0.0, 255.0) as u8,
    )
}

fn rgba_to_yuv420(width: u32, height: u32, rgba: &[u8]) -> Result<Yuv420Owned> {
    if width == 0 || height == 0 {
        return Err(ScreenRecorderError::Encode(
            "H264 encoder requires non-zero dimensions".to_string(),
        ));
    }

    if width % 2 != 0 || height % 2 != 0 {
        return Err(ScreenRecorderError::Encode(
            "H264 lossless encoder requires even width/height".to_string(),
        ));
    }

    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(ScreenRecorderError::Encode(
            "RGBA input size mismatch".to_string(),
        ));
    }

    let y_stride = next_multiple(width, 16) as usize;
    let y_rows = next_multiple(height, 16) as usize;

    let c_width = width / 2;
    let c_height = height / 2;
    let c_stride = next_multiple(c_width, 8) as usize;
    let c_rows = next_multiple(c_height, 8) as usize;

    let mut y = vec![0u8; y_stride * y_rows];
    let mut cb = vec![128u8; c_stride * c_rows];
    let mut cr = vec![128u8; c_stride * c_rows];

    for py in 0..height as usize {
        for px in 0..width as usize {
            let src = (py * width as usize + px) * 4;
            let (yy, _, _) = rgb_to_yuv(rgba[src], rgba[src + 1], rgba[src + 2]);
            y[py * y_stride + px] = yy;
        }
    }

    for py in (0..height as usize).step_by(2) {
        for px in (0..width as usize).step_by(2) {
            let mut sum_cb = 0f32;
            let mut sum_cr = 0f32;

            for oy in 0..2 {
                for ox in 0..2 {
                    let sx = px + ox;
                    let sy = py + oy;
                    let src = (sy * width as usize + sx) * 4;
                    let (_, cc_b, cc_r) = rgb_to_yuv(rgba[src], rgba[src + 1], rgba[src + 2]);
                    sum_cb += cc_b as f32;
                    sum_cr += cc_r as f32;
                }
            }

            let chroma_x = px / 2;
            let chroma_y = py / 2;
            cb[chroma_y * c_stride + chroma_x] = (sum_cb / 4.0).round() as u8;
            cr[chroma_y * c_stride + chroma_x] = (sum_cr / 4.0).round() as u8;
        }
    }

    Ok(Yuv420Owned {
        width,
        height,
        y_stride,
        c_stride,
        y,
        cb,
        cr,
    })
}

pub struct H264LosslessFileWriter {
    inner: H264Writer<BufWriter<File>>,
}

impl H264LosslessFileWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        let inner = H264Writer::new(writer).map_err(|e| {
            ScreenRecorderError::Encode(format!("failed to create H264 writer: {e}"))
        })?;
        Ok(Self { inner })
    }

    pub fn write_rgba_frame(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        let yuv = rgba_to_yuv420(width, height, rgba)?;
        let image = yuv.as_image();
        self.inner
            .write(&image)
            .map_err(|e| ScreenRecorderError::Encode(format!("failed to encode H264 frame: {e}")))
    }

    pub fn flush(self) -> Result<()> {
        let mut writer = self.inner.into_inner();
        writer.flush()?;
        Ok(())
    }
}

pub struct H264LosslessAnnexBEncoder {
    encoder: Option<LessEncoder>,
}

impl H264LosslessAnnexBEncoder {
    pub fn new() -> Self {
        Self { encoder: None }
    }

    pub fn encode_rgba(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
        let yuv = rgba_to_yuv420(width, height, rgba)?;
        let image = yuv.as_image();

        if let Some(encoder) = self.encoder.as_mut() {
            let nal = encoder.encode(&image).map_err(|e| {
                ScreenRecorderError::Encode(format!("H264 frame encode failed: {e}"))
            })?;
            Ok(nal.to_annex_b_data())
        } else {
            let (initial, encoder) = LessEncoder::new(&image).map_err(|e| {
                ScreenRecorderError::Encode(format!("failed to initialize H264 encoder: {e}"))
            })?;
            self.encoder = Some(encoder);
            Ok(initial_to_annex_b(initial))
        }
    }
}

fn initial_to_annex_b(initial: InitialNalUnits) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&initial.sps.to_annex_b_data());
    out.extend_from_slice(&initial.pps.to_annex_b_data());
    out.extend_from_slice(&initial.frame.to_annex_b_data());
    out
}
