use snow_audio_recorder::AudioEvent;

use crate::event::{AudioCaptureEvent, RecordingEvent, StreamTimestamp};

/// Create an audio event mapper closure for use with `StreamBridge`.
///
/// Converts `AudioEvent` variants into `RecordingEvent::Audio(...)`.
pub(crate) fn create_audio_mapper() -> impl Fn(AudioEvent) -> RecordingEvent + Send + 'static {
    move |event: AudioEvent| -> RecordingEvent {
        #[allow(deprecated)]
        match event {
            AudioEvent::Packet(pkt) => {
                let ts = StreamTimestamp {
                    instant: pkt
                        .metadata
                        .capture_time
                        .unwrap_or_else(std::time::Instant::now),
                    qpc_100ns: pkt.metadata.qpc_position_100ns,
                };

                RecordingEvent::Audio(AudioCaptureEvent::Packet {
                    source: pkt.source,
                    data: pkt.data,
                    frames: pkt.frames,
                    format: pkt.format,
                    timestamp: ts,
                })
            }

            AudioEvent::PacketDropped {
                source,
                dropped_frames,
            } => RecordingEvent::Audio(AudioCaptureEvent::PacketDropped {
                source,
                dropped_frames,
            }),

            AudioEvent::SourceRestarted {
                source,
                old_device_id,
                new_device_id,
                downtime,
            } => RecordingEvent::Audio(AudioCaptureEvent::SourceRestarted {
                source,
                old_device_id,
                new_device_id,
                downtime,
            }),

            AudioEvent::Paused { at } => RecordingEvent::Audio(AudioCaptureEvent::Paused { at }),

            AudioEvent::Resumed { at, gap } => {
                RecordingEvent::Audio(AudioCaptureEvent::Resumed { at, gap })
            }

            AudioEvent::BufferPressure {
                fill_ratio,
                buffer_depth,
            } => RecordingEvent::Audio(AudioCaptureEvent::BufferPressure {
                fill_ratio,
                buffer_depth,
            }),

            AudioEvent::StreamEnded => RecordingEvent::Audio(AudioCaptureEvent::StreamEnded),

            AudioEvent::Error(err) => {
                RecordingEvent::Audio(AudioCaptureEvent::Error(Box::new(err)))
            }
        }
    }
}
