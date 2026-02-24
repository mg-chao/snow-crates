use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, ScreenRecorderError};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredFrame {
    pub timestamp_ms: u64,
    pub duration_ms: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn write_frames(path: &Path, frames: &[StoredFrame]) -> Result<()> {
    let file = File::create(path)?;
    bincode::serialize_into(BufWriter::new(file), frames)
        .map_err(|err| ScreenRecorderError::Io(std::io::Error::other(err)))
}

pub fn read_frames(path: &Path) -> Result<Vec<StoredFrame>> {
    let file = File::open(path)?;
    bincode::deserialize_from(BufReader::new(file))
        .map_err(|err| ScreenRecorderError::Decode(format!("failed to decode frame store: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_store_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "snow-screen-recorder-frame-store-{}.bin",
            uuid::Uuid::new_v4().simple()
        ));

        let frames = vec![StoredFrame {
            timestamp_ms: 0,
            duration_ms: 16,
            width: 2,
            height: 2,
            rgba: vec![0, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255],
        }];

        write_frames(&path, &frames).expect("write should succeed");
        let decoded = read_frames(&path).expect("read should succeed");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].duration_ms, 16);

        let _ = std::fs::remove_file(path);
    }
}
