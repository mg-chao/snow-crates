use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Debug)]
pub struct AviAudioTrack {
    pub mp3_data: Vec<u8>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bitrate_kbps: u16,
}

#[derive(Clone, Debug)]
struct IndexEntry {
    chunk_id: [u8; 4],
    flags: u32,
    offset: u32,
    size: u32,
}

fn fourcc(tag: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*tag)
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_i16(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len() + 1);
    out.extend_from_slice(tag);
    write_u32(&mut out, payload.len() as u32);
    out.extend_from_slice(payload);
    if payload.len() % 2 != 0 {
        out.push(0);
    }
    out
}

fn list_chunk(list_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len() + 1);
    out.extend_from_slice(b"LIST");
    write_u32(&mut out, (4 + payload.len()) as u32);
    out.extend_from_slice(list_type);
    out.extend_from_slice(payload);
    if payload.len() % 2 != 0 {
        out.push(0);
    }
    out
}

fn build_main_header(
    width: u32,
    height: u32,
    fps: u32,
    total_frames: u32,
    stream_count: u32,
    max_chunk_size: u32,
) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    write_u32(&mut avih, 1_000_000u32 / fps.max(1));
    write_u32(&mut avih, max_chunk_size.saturating_mul(fps));
    write_u32(&mut avih, 0);
    write_u32(&mut avih, 0x10); // AVIF_HASINDEX
    write_u32(&mut avih, total_frames);
    write_u32(&mut avih, 0);
    write_u32(&mut avih, stream_count);
    write_u32(&mut avih, max_chunk_size);
    write_u32(&mut avih, width);
    write_u32(&mut avih, height);
    write_u32(&mut avih, 0);
    write_u32(&mut avih, 0);
    write_u32(&mut avih, 0);
    write_u32(&mut avih, 0);
    avih
}

fn build_video_stream_list(
    width: u32,
    height: u32,
    fps: u32,
    total_frames: u32,
    max_frame_size: u32,
) -> Vec<u8> {
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids");
    strh.extend_from_slice(b"MJPG");
    write_u32(&mut strh, 0);
    write_u16(&mut strh, 0);
    write_u16(&mut strh, 0);
    write_u32(&mut strh, 0);
    write_u32(&mut strh, 1);
    write_u32(&mut strh, fps.max(1));
    write_u32(&mut strh, 0);
    write_u32(&mut strh, total_frames);
    write_u32(&mut strh, max_frame_size);
    write_u32(&mut strh, u32::MAX);
    write_u32(&mut strh, 0);
    write_i16(&mut strh, 0);
    write_i16(&mut strh, 0);
    write_i16(&mut strh, width as i16);
    write_i16(&mut strh, height as i16);

    let mut strf = Vec::with_capacity(40);
    write_u32(&mut strf, 40);
    write_u32(&mut strf, width);
    write_u32(&mut strf, height);
    write_u16(&mut strf, 1);
    write_u16(&mut strf, 24);
    write_u32(&mut strf, fourcc(b"MJPG"));
    write_u32(&mut strf, width.saturating_mul(height).saturating_mul(3));
    write_u32(&mut strf, 0);
    write_u32(&mut strf, 0);
    write_u32(&mut strf, 0);
    write_u32(&mut strf, 0);

    let mut strl_payload = Vec::new();
    strl_payload.extend_from_slice(&chunk(b"strh", &strh));
    strl_payload.extend_from_slice(&chunk(b"strf", &strf));
    list_chunk(b"strl", &strl_payload)
}

fn build_audio_stream_list(track: &AviAudioTrack) -> Vec<u8> {
    let avg_bytes_per_sec = u32::from(track.bitrate_kbps).saturating_mul(125);

    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"auds");
    strh.extend_from_slice(&0u32.to_le_bytes());
    write_u32(&mut strh, 0);
    write_u16(&mut strh, 0);
    write_u16(&mut strh, 0);
    write_u32(&mut strh, 0);
    write_u32(&mut strh, 1);
    write_u32(&mut strh, track.sample_rate_hz);
    write_u32(&mut strh, 0);
    write_u32(&mut strh, track.mp3_data.len() as u32);
    write_u32(&mut strh, avg_bytes_per_sec.max(1));
    write_u32(&mut strh, u32::MAX);
    write_u32(&mut strh, 0);
    write_i16(&mut strh, 0);
    write_i16(&mut strh, 0);
    write_i16(&mut strh, 0);
    write_i16(&mut strh, 0);

    let mut strf = Vec::new();
    write_u16(&mut strf, 0x55); // WAVE_FORMAT_MPEGLAYER3
    write_u16(&mut strf, track.channels);
    write_u32(&mut strf, track.sample_rate_hz);
    write_u32(&mut strf, avg_bytes_per_sec.max(1));
    write_u16(&mut strf, 1);
    write_u16(&mut strf, 0);
    write_u16(&mut strf, 0);

    let mut strl_payload = Vec::new();
    strl_payload.extend_from_slice(&chunk(b"strh", &strh));
    strl_payload.extend_from_slice(&chunk(b"strf", &strf));
    list_chunk(b"strl", &strl_payload)
}

pub fn write_avi(
    output_path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    mjpeg_frames: &[Vec<u8>],
    audio: Option<&AviAudioTrack>,
) -> Result<()> {
    if mjpeg_frames.is_empty() {
        return Err(ScreenRecorderError::Export(
            "AVI export requires at least one video frame".to_string(),
        ));
    }

    let max_frame_size = mjpeg_frames
        .iter()
        .map(|f| f.len() as u32)
        .max()
        .unwrap_or(0);

    let stream_count = if audio.is_some() { 2 } else { 1 };

    let mut hdrl_payload = Vec::new();
    let avih = build_main_header(
        width,
        height,
        fps,
        mjpeg_frames.len() as u32,
        stream_count,
        max_frame_size,
    );
    hdrl_payload.extend_from_slice(&chunk(b"avih", &avih));
    hdrl_payload.extend_from_slice(&build_video_stream_list(
        width,
        height,
        fps,
        mjpeg_frames.len() as u32,
        max_frame_size,
    ));
    if let Some(audio_track) = audio {
        hdrl_payload.extend_from_slice(&build_audio_stream_list(audio_track));
    }
    let hdrl = list_chunk(b"hdrl", &hdrl_payload);

    let mut movi_payload = Vec::new();
    let mut idx_entries = Vec::<IndexEntry>::new();
    let mut movi_offset: u32 = 4; // starts after the 'movi' type field

    for frame in mjpeg_frames {
        let chunk_bytes = chunk(b"00dc", frame);
        idx_entries.push(IndexEntry {
            chunk_id: *b"00dc",
            flags: 0x10,
            offset: movi_offset,
            size: frame.len() as u32,
        });
        movi_offset = movi_offset.saturating_add(chunk_bytes.len() as u32);
        movi_payload.extend_from_slice(&chunk_bytes);
    }

    if let Some(audio_track) = audio {
        if !audio_track.mp3_data.is_empty() {
            let chunk_bytes = chunk(b"01wb", &audio_track.mp3_data);
            idx_entries.push(IndexEntry {
                chunk_id: *b"01wb",
                flags: 0,
                offset: movi_offset,
                size: audio_track.mp3_data.len() as u32,
            });
            movi_payload.extend_from_slice(&chunk_bytes);
        }
    }

    let movi = list_chunk(b"movi", &movi_payload);

    let mut idx1_payload = Vec::with_capacity(idx_entries.len() * 16);
    for entry in idx_entries {
        idx1_payload.extend_from_slice(&entry.chunk_id);
        write_u32(&mut idx1_payload, entry.flags);
        write_u32(&mut idx1_payload, entry.offset);
        write_u32(&mut idx1_payload, entry.size);
    }
    let idx1 = chunk(b"idx1", &idx1_payload);

    let mut riff_payload = Vec::new();
    riff_payload.extend_from_slice(b"AVI ");
    riff_payload.extend_from_slice(&hdrl);
    riff_payload.extend_from_slice(&movi);
    riff_payload.extend_from_slice(&idx1);

    let mut file = File::create(output_path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&(riff_payload.len() as u32).to_le_bytes())?;
    file.write_all(&riff_payload)?;
    file.flush()?;
    Ok(())
}
