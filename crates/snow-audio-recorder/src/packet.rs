use std::time::{Duration, Instant};

use crate::error::AudioError;
use crate::format::AudioFormat;
use snow_core::event::StreamEvent;
use snow_core::timestamp::{StreamTimestamp, TickFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceKind {
    System,
    Microphone,
}

#[derive(Clone, Debug, Default)]
pub struct AudioPacketMetadata {
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
    /// Populates `stream_timestamp` from the capture time and QPC value.
    pub(crate) fn set_timing(&mut self, capture_time: Option<Instant>, qpc_position_100ns: Option<i64>) {
        self.stream_timestamp = Some(StreamTimestamp {
            instant: capture_time.unwrap_or_else(Instant::now),
            raw_os_ticks: qpc_position_100ns,
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
    pub fn end_capture_time(&self) -> Option<Instant> {
        self.metadata.stream_timestamp.as_ref().map(|st| st.instant)
    }

    /// Wall-clock `Instant` at packet start.
    pub fn start_capture_time(&self) -> Option<Instant> {
        self.end_capture_time()
            .and_then(|end| end.checked_sub(self.duration()))
    }

    /// QPC position (100ns units) at packet end.
    pub fn end_qpc_position_100ns(&self) -> Option<i64> {
        self.metadata.stream_timestamp.as_ref().and_then(|st| st.raw_os_ticks)
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

impl StreamEvent for AudioEvent {
    fn is_paused(&self) -> bool {
        matches!(self, AudioEvent::Paused { .. })
    }

    fn is_resumed(&self) -> bool {
        matches!(self, AudioEvent::Resumed { .. })
    }

    fn is_stream_ended(&self) -> bool {
        matches!(self, AudioEvent::StreamEnded)
    }

    fn is_error(&self) -> bool {
        matches!(self, AudioEvent::Error(_))
    }

    fn timestamp(&self) -> Option<&StreamTimestamp> {
        match self {
            AudioEvent::Packet(packet) => packet.metadata.stream_timestamp.as_ref(),
            _ => None,
        }
    }
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
        packet.metadata.set_timing(None, Some(1_000_000));

        assert_eq!(packet.duration(), Duration::from_millis(20));
        assert_eq!(packet.duration_100ns(), 200_000);
        assert_eq!(packet.start_qpc_position_100ns(), Some(800_000));
    }

    #[test]
    fn packet_start_capture_time_is_duration_before_end() {
        let mut packet = make_packet(4_800);
        let end = Instant::now();
        packet.metadata.set_timing(Some(end), None);

        let start = packet
            .start_capture_time()
            .expect("start capture time should be derivable");
        assert_eq!(
            end.saturating_duration_since(start),
            Duration::from_millis(100)
        );
    }

    /// Generate a random `AudioPacket` with optional `stream_timestamp`.
    fn arb_audio_packet() -> impl Strategy<Value = AudioPacket> {
        let format = AudioFormat::new(48_000, 2, AudioSampleFormat::I16);
        let has_ts = proptest::bool::ANY;
        (0u32..960, has_ts).prop_map(move |(frames, with_ts)| {
            let bytes = format.bytes_for_frames(frames).unwrap_or(0);
            let mut metadata = AudioPacketMetadata::default();
            if with_ts {
                metadata.stream_timestamp = Some(StreamTimestamp {
                    instant: Instant::now(),
                    raw_os_ticks: Some(42),
                    tick_format: TickFormat::Hns100,
                });
            }
            AudioPacket {
                source: AudioSourceKind::System,
                format,
                frames,
                data: vec![0; bytes],
                metadata,
            }
        })
    }

    /// Generate a random `AudioEvent` variant covering all eight variants.
    fn arb_audio_event() -> impl Strategy<Value = AudioEvent> {
        prop_oneof![
            // Packet (data variant)
            arb_audio_packet().prop_map(AudioEvent::Packet),
            // PacketDropped (control variant)
            (0u64..1000).prop_map(|dropped| AudioEvent::PacketDropped {
                source: AudioSourceKind::System,
                dropped_frames: dropped,
            }),
            // SourceRestarted (control variant)
            Just(AudioEvent::SourceRestarted {
                source: AudioSourceKind::Microphone,
                old_device_id: None,
                new_device_id: "new-dev".to_string(),
                downtime: Duration::from_millis(50),
            }),
            // Paused (lifecycle)
            (0u64..10000).prop_map(|_| AudioEvent::Paused { at: Instant::now() }),
            // Resumed (lifecycle)
            (0u64..10000).prop_map(|gap_ms| AudioEvent::Resumed {
                at: Instant::now(),
                gap: Duration::from_millis(gap_ms),
            }),
            // BufferPressure (control variant)
            (0.0f64..=1.0, 1usize..256).prop_map(|(fill, depth)| AudioEvent::BufferPressure {
                fill_ratio: fill,
                buffer_depth: depth,
            }),
            // StreamEnded (lifecycle)
            Just(AudioEvent::StreamEnded),
            // Error (lifecycle)
            Just(AudioEvent::Error(AudioError::DeviceLost)),
        ]
    }

    // **Validates: Requirements 1.2, 1.3**
    //
    // Property 2: Tick format matches source type
    //
    // For any `AudioPacketMetadata` with timing set (any combination of
    // `capture_time` and QPC values including `None`),
    // `stream_timestamp.tick_format` SHALL be `TickFormat::Hns100`.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]
        #[test]
        fn prop_audio_metadata_tick_format_always_hns100(
            has_capture_time in proptest::bool::ANY,
            has_qpc in proptest::bool::ANY,
            qpc_value in proptest::num::i64::ANY,
        ) {
            let capture_time = if has_capture_time { Some(Instant::now()) } else { None };
            let qpc = if has_qpc { Some(qpc_value) } else { None };

            let mut meta = AudioPacketMetadata::default();
            meta.set_timing(capture_time, qpc);

            let ts = meta.stream_timestamp.as_ref()
                .expect("stream_timestamp must be Some after set_timing");
            prop_assert_eq!(ts.tick_format, TickFormat::Hns100,
                "AudioPacketMetadata tick_format must always be Hns100, got {:?}", ts.tick_format);
        }
    }

    // **Validates: Requirements 1.6, 1.7, 7.2**
    //
    // Property 1: Leaf metadata always populates stream_timestamp
    //
    // For any call to `AudioPacketMetadata::set_timing` with any combination
    // of `capture_time` (Some/None) and QPC values (Some/None), the resulting
    // `stream_timestamp` SHALL be `Some` with a valid `Instant`.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_audio_metadata_always_populates_stream_timestamp(
            has_capture_time in proptest::bool::ANY,
            has_qpc in proptest::bool::ANY,
            qpc_value in proptest::num::i64::ANY,
        ) {
            let capture_time = if has_capture_time { Some(Instant::now()) } else { None };
            let qpc = if has_qpc { Some(qpc_value) } else { None };

            let mut meta = AudioPacketMetadata::default();
            let before = Instant::now();
            meta.set_timing(capture_time, qpc);
            let after = Instant::now();

            // stream_timestamp must always be Some
            let ts = meta.stream_timestamp.as_ref();
            prop_assert!(ts.is_some(), "stream_timestamp must always be Some after set_timing");

            let ts = ts.unwrap();

            // instant must be valid (between before and after, or equal to capture_time)
            if let Some(ct) = capture_time {
                prop_assert_eq!(ts.instant, ct,
                    "instant should equal the provided capture_time");
            } else {
                // When capture_time is None, instant is Instant::now() at call time
                prop_assert!(ts.instant >= before,
                    "instant should be >= time before set_timing call");
                prop_assert!(ts.instant <= after,
                    "instant should be <= time after set_timing call");
            }

            // raw_os_ticks should match the provided QPC
            prop_assert_eq!(ts.raw_os_ticks, qpc,
                "raw_os_ticks should match the provided QPC value");
        }
    }

    // **Validates: Requirements 3.4, 3.6**
    //
    // Property 3: StreamEvent lifecycle consistency (AudioEvent)
    //
    // For lifecycle variants (Paused, Resumed, StreamEnded, Error), exactly
    // one lifecycle method returns true. For data/control variants (Packet,
    // PacketDropped, SourceRestarted, BufferPressure), all lifecycle methods
    // return false.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_audio_event_lifecycle_consistency(event in arb_audio_event()) {
            let lifecycle_results = [
                event.is_paused(),
                event.is_resumed(),
                event.is_stream_ended(),
                event.is_error(),
            ];
            let true_count = lifecycle_results.iter().filter(|&&v| v).count();

            let is_lifecycle_variant = matches!(
                event,
                AudioEvent::Paused { .. }
                    | AudioEvent::Resumed { .. }
                    | AudioEvent::StreamEnded
                    | AudioEvent::Error(_)
            );

            if is_lifecycle_variant {
                // Exactly one lifecycle method returns true
                prop_assert_eq!(
                    true_count, 1,
                    "lifecycle variant should have exactly one true method, got {}",
                    true_count
                );
            } else {
                // Data/control variants: all lifecycle methods return false
                prop_assert_eq!(
                    true_count, 0,
                    "data/control variant should have all false lifecycle methods, got {} true",
                    true_count
                );
            }

            // Verify specific lifecycle method matches the variant
            match &event {
                AudioEvent::Paused { .. } => {
                    prop_assert!(event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                AudioEvent::Resumed { .. } => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                AudioEvent::StreamEnded => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(event.is_stream_ended());
                    prop_assert!(!event.is_error());
                }
                AudioEvent::Error(_) => {
                    prop_assert!(!event.is_paused());
                    prop_assert!(!event.is_resumed());
                    prop_assert!(!event.is_stream_ended());
                    prop_assert!(event.is_error());
                }
                _ => {}
            }

            // Verify timestamp() behavior:
            // - Packet with stream_timestamp set → Some
            // - All other variants → None
            match &event {
                AudioEvent::Packet(packet) => {
                    if packet.metadata.stream_timestamp.is_some() {
                        prop_assert!(event.timestamp().is_some(),
                            "Packet with stream_timestamp should return Some from timestamp()");
                    } else {
                        prop_assert!(event.timestamp().is_none(),
                            "Packet without stream_timestamp should return None from timestamp()");
                    }
                }
                _ => {
                    prop_assert!(event.timestamp().is_none(),
                        "Non-Packet variant should return None from timestamp()");
                }
            }
        }
    }
}
