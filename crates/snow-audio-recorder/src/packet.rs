use std::time::{Duration, Instant};

use crate::error::AudioError;
use crate::format::AudioFormat;
use snow_core::timestamp::{StreamTimestamp, TickFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceKind {
    System,
    Microphone,
}

#[derive(Clone, Debug, Default)]
pub struct AudioPacketMetadata {
    /// Approximate wall-clock `Instant` at the end of this packet.
    /// Derived from the most recent source chunk that contributes to
    /// the packet.
    #[deprecated(note = "Use `stream_timestamp.instant` instead")]
    pub capture_time: Option<Instant>,
    /// WASAPI QPC position (100ns units) at packet end.
    #[deprecated(note = "Use `stream_timestamp` instead")]
    pub qpc_position_100ns: Option<i64>,
    /// Device position in source frames at packet end.
    pub device_position_frames: Option<u64>,
    pub discontinuity: bool,
    pub is_silent: bool,
    pub sequence: u64,
    /// Unified timestamp. `tick_format` is `Hns100`.
    pub stream_timestamp: Option<StreamTimestamp>,
}

impl AudioPacketMetadata {
    /// Set timing fields from a capture operation.
    ///
    /// Populates both the deprecated `capture_time` / `qpc_position_100ns`
    /// fields and the new `stream_timestamp` field so that old and new
    /// consumers both see correct values.
    #[allow(deprecated)]
    pub(crate) fn set_timing(&mut self, capture_time: Option<Instant>, qpc_position_100ns: Option<i64>) {
        self.capture_time = capture_time;
        self.qpc_position_100ns = qpc_position_100ns;
        self.stream_timestamp = qpc_position_100ns.map(|qpc| StreamTimestamp {
            instant: capture_time.unwrap_or_else(Instant::now),
            raw_os_ticks: Some(qpc),
            tick_format: TickFormat::Hns100,
        });
    }
}

#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub source: AudioSourceKind,
    pub format: AudioFormat,
    pub frames: u32,
    pub data: Vec<u8>,
    pub metadata: AudioPacketMetadata,
}

impl AudioPacket {
    /// Duration covered by this packet's audio payload.
    pub fn duration(&self) -> Duration {
        frames_to_duration(self.frames, self.format.sample_rate)
    }

    /// Duration covered by this packet expressed in 100ns units.
    pub fn duration_100ns(&self) -> i64 {
        frames_to_100ns(self.frames, self.format.sample_rate)
    }

    /// Wall-clock `Instant` at packet end.
    #[allow(deprecated)]
    pub fn end_capture_time(&self) -> Option<Instant> {
        self.metadata.capture_time
    }

    /// Wall-clock `Instant` at packet start.
    pub fn start_capture_time(&self) -> Option<Instant> {
        self.end_capture_time()
            .and_then(|end| end.checked_sub(self.duration()))
    }

    /// QPC position (100ns units) at packet end.
    #[allow(deprecated)]
    pub fn end_qpc_position_100ns(&self) -> Option<i64> {
        self.metadata.qpc_position_100ns
    }

    /// QPC position (100ns units) at packet start.
    pub fn start_qpc_position_100ns(&self) -> Option<i64> {
        self.end_qpc_position_100ns()
            .map(|end| end.saturating_sub(self.duration_100ns()))
    }
}

pub(crate) fn frames_to_duration(frames: u32, sample_rate: u32) -> Duration {
    if frames == 0 || sample_rate == 0 {
        return Duration::ZERO;
    }
    let nanos = u128::from(frames)
        .saturating_mul(1_000_000_000u128)
        .checked_div(u128::from(sample_rate))
        .unwrap_or(0);
    Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

pub(crate) fn frames_to_100ns(frames: u32, sample_rate: u32) -> i64 {
    if frames == 0 || sample_rate == 0 {
        return 0;
    }
    let hns = u128::from(frames)
        .saturating_mul(10_000_000u128)
        .checked_div(u128::from(sample_rate))
        .unwrap_or(0);
    hns.min(i64::MAX as u128) as i64
}

#[derive(Clone, Debug)]
pub enum AudioEvent {
    Packet(AudioPacket),
    PacketDropped {
        source: AudioSourceKind,
        dropped_frames: u64,
    },
    SourceRestarted {
        source: AudioSourceKind,
        old_device_id: Option<String>,
        new_device_id: String,
        downtime: Duration,
    },
    Paused {
        at: Instant,
    },
    Resumed {
        at: Instant,
        gap: Duration,
    },
    /// Proactive backpressure signal emitted when the data-lane fill ratio
    /// crosses the configured threshold. Lets consumers adapt (reduce
    /// processing, log warnings) before drops actually happen.
    BufferPressure {
        /// Current fill ratio in `0.0..=1.0`.
        fill_ratio: f64,
        /// Configured capacity of the data-event lane.
        buffer_depth: usize,
    },
    StreamEnded,
    Error(AudioError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::AudioSampleFormat;
    use proptest::prelude::*;
    use snow_core::timestamp::TickFormat;

    fn make_packet(frames: u32) -> AudioPacket {
        let format = AudioFormat::new(48_000, 2, AudioSampleFormat::I16);
        let bytes = format
            .bytes_for_frames(frames)
            .expect("test format should be valid");
        AudioPacket {
            source: AudioSourceKind::System,
            format,
            frames,
            data: vec![0; bytes],
            metadata: AudioPacketMetadata::default(),
        }
    }

    #[test]
    #[allow(deprecated)]
    fn packet_duration_and_qpc_range_are_consistent() {
        let mut packet = make_packet(960);
        packet.metadata.qpc_position_100ns = Some(1_000_000);

        assert_eq!(packet.duration(), Duration::from_millis(20));
        assert_eq!(packet.duration_100ns(), 200_000);
        assert_eq!(packet.start_qpc_position_100ns(), Some(800_000));
    }

    #[test]
    #[allow(deprecated)]
    fn packet_start_capture_time_is_duration_before_end() {
        let mut packet = make_packet(4_800);
        let end = Instant::now();
        packet.metadata.capture_time = Some(end);

        let start = packet
            .start_capture_time()
            .expect("start capture time should be derivable");
        assert_eq!(
            end.saturating_duration_since(start),
            Duration::from_millis(100)
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]
        #[test]
        fn prop_audio_metadata_hns100_timestamp(qpc_value in proptest::num::i64::ANY) {
            let mut meta = AudioPacketMetadata::default();
            meta.set_timing(Some(Instant::now()), Some(qpc_value));

            let ts = meta.stream_timestamp.as_ref().unwrap();
            prop_assert_eq!(ts.tick_format, TickFormat::Hns100);
            prop_assert_eq!(ts.raw_os_ticks, Some(qpc_value));
        }
    }
}
