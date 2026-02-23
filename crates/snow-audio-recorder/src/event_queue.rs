use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::error::{RecvError, RecvTimeoutError, TryRecvError};
use crate::packet::AudioEvent;

/// Returns `true` for control-plane events that must never be silently dropped.
/// Data-plane events (`Packet`, `PacketDropped`) are bounded and droppable.
/// `BufferPressure` is a control event — it rides the unbounded lane so it
/// is never evicted by the very pressure it reports.
pub(crate) fn is_control_event(event: &AudioEvent) -> bool {
    !matches!(
        event,
        AudioEvent::Packet(_) | AudioEvent::PacketDropped { .. } | AudioEvent::StreamEnded
    )
}

struct QueueState {
    closed: bool,
    /// Bounded ring-buffer for data-plane events (Packet / PacketDropped).
    data_events: VecDeque<AudioEvent>,
    /// Unbounded queue for control-plane signals (Paused, Resumed, Error, …).
    /// These are rare but must never be evicted by packet pressure.
    control_events: VecDeque<AudioEvent>,
}

impl QueueState {
    fn is_empty(&self) -> bool {
        self.control_events.is_empty() && self.data_events.is_empty()
    }

    /// Pop the next event, prioritising control events over data events.
    fn pop_next(&mut self) -> Option<AudioEvent> {
        self.control_events
            .pop_front()
            .or_else(|| self.data_events.pop_front())
    }

    fn data_len(&self) -> usize {
        self.data_events.len()
    }

    /// Total number of pending events across both lanes.
    fn total_len(&self) -> usize {
        self.control_events.len() + self.data_events.len()
    }
}

pub(crate) struct PushOutcome {
    pub dropped: Option<AudioEvent>,
    /// Number of buffered events in the bounded data lane.
    pub len: usize,
}

pub(crate) struct EventQueue {
    /// Maximum capacity for the data-event lane.
    data_depth: usize,
    state: Mutex<QueueState>,
    cv: Condvar,
}

impl EventQueue {
    pub fn new(depth: usize) -> Self {
        let depth = depth.max(1);
        Self {
            data_depth: depth,
            state: Mutex::new(QueueState {
                closed: false,
                data_events: VecDeque::with_capacity(depth),
                control_events: VecDeque::new(),
            }),
            cv: Condvar::new(),
        }
    }

    pub fn push(&self, event: AudioEvent) -> PushOutcome {
        let mut guard = self.state.lock().unwrap();
        if guard.closed {
            return PushOutcome {
                dropped: Some(event),
                len: guard.data_len(),
            };
        }

        if is_control_event(&event) {
            // Control events go into the unbounded lane — never dropped.
            guard.control_events.push_back(event);
            let len = guard.data_len();
            self.cv.notify_one();
            return PushOutcome { dropped: None, len };
        }

        // Data-plane: bounded, oldest-eviction policy.
        let dropped = if guard.data_events.len() >= self.data_depth {
            guard.data_events.pop_front()
        } else {
            None
        };

        guard.data_events.push_back(event);
        let len = guard.data_len();
        self.cv.notify_one();
        PushOutcome { dropped, len }
    }

    pub fn recv(&self) -> Result<(AudioEvent, usize), RecvError> {
        let mut guard = self.state.lock().unwrap();
        loop {
            if let Some(event) = guard.pop_next() {
                return Ok((event, guard.data_len()));
            }
            if guard.closed {
                return Err(RecvError);
            }
            guard = self.cv.wait(guard).unwrap();
        }
    }

    pub fn try_recv(&self) -> Result<(AudioEvent, usize), TryRecvError> {
        let mut guard = self.state.lock().unwrap();
        if let Some(event) = guard.pop_next() {
            return Ok((event, guard.data_len()));
        }
        if guard.closed {
            return Err(TryRecvError::Closed);
        }
        Err(TryRecvError::Empty)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<(AudioEvent, usize), RecvTimeoutError> {
        let mut guard = self.state.lock().unwrap();
        if let Some(event) = guard.pop_next() {
            return Ok((event, guard.data_len()));
        }

        if guard.closed {
            return Err(RecvTimeoutError::Closed);
        }

        let (mut guard, wait_result) = self
            .cv
            .wait_timeout_while(guard, timeout, |state| !state.closed && state.is_empty())
            .unwrap();

        if let Some(event) = guard.pop_next() {
            return Ok((event, guard.data_len()));
        }

        if guard.closed {
            return Err(RecvTimeoutError::Closed);
        }

        if wait_result.timed_out() {
            Err(RecvTimeoutError::Timeout)
        } else {
            Err(RecvTimeoutError::Closed)
        }
    }

    pub fn close(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        self.cv.notify_all();
    }

    pub fn drain(&self) -> Vec<AudioEvent> {
        let mut guard = self.state.lock().unwrap();
        let mut drained = Vec::with_capacity(guard.total_len());
        // Drain control events first so they appear before any remaining data events.
        while let Some(event) = guard.control_events.pop_front() {
            drained.push(event);
        }
        while let Some(event) = guard.data_events.pop_front() {
            drained.push(event);
        }
        drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    use crate::error::AudioError;
    use crate::format::{AudioFormat, AudioSampleFormat};
    use crate::packet::{AudioPacket, AudioPacketMetadata, AudioSourceKind};
    use crate::streaming::AudioStreamStats;

    fn packet(seq: u64) -> AudioPacket {
        AudioPacket {
            source: AudioSourceKind::System,
            format: AudioFormat::new(48_000, 2, AudioSampleFormat::F32),
            frames: 480,
            data: vec![0; 480 * 2 * 4],
            metadata: AudioPacketMetadata {
                sequence: seq,
                ..Default::default()
            },
        }
    }

    fn push_event_with_drop_notice(
        queue: &EventQueue,
        stats: &AudioStreamStats,
        event: AudioEvent,
    ) {
        let outcome = queue.push(event);
        stats
            .buffer_fill
            .store(outcome.len as u64, Ordering::Release);

        if let Some(dropped) = outcome.dropped {
            if let AudioEvent::Packet(AudioPacket { source, frames, .. }) = &dropped {
                let dropped_frames = *frames as u64;
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                stats
                    .frames_dropped
                    .fetch_add(dropped_frames, Ordering::Relaxed);
                let notice_outcome = queue.push(AudioEvent::PacketDropped {
                    source: *source,
                    dropped_frames,
                });
                stats
                    .buffer_fill
                    .store(notice_outcome.len as u64, Ordering::Release);
                if let Some(AudioEvent::Packet(pkt)) = notice_outcome.dropped {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    stats
                        .frames_dropped
                        .fetch_add(pkt.frames as u64, Ordering::Relaxed);
                }
            }
        }
    }

    #[test]
    fn bounded_queue_drops_oldest_packet() {
        let queue = EventQueue::new(2);
        let stats = AudioStreamStats::default();

        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(1)));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(2)));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(3)));

        let first = queue.recv().unwrap().0;
        let second = queue.recv().unwrap().0;

        match first {
            AudioEvent::Packet(pkt) => assert_eq!(pkt.metadata.sequence, 3),
            _ => panic!("expected packet event"),
        }

        match second {
            AudioEvent::PacketDropped {
                source,
                dropped_frames,
            } => {
                assert_eq!(source, AudioSourceKind::System);
                assert_eq!(dropped_frames, 480);
            }
            _ => panic!("expected packet dropped event"),
        }
    }

    #[test]
    fn control_events_survive_data_lane_saturation() {
        let queue = EventQueue::new(2);
        let stats = AudioStreamStats::default();

        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(1)));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(2)));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Error(AudioError::DeviceLost));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(3)));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(4)));

        let first = queue.recv().unwrap().0;
        assert!(
            matches!(first, AudioEvent::Error(_)),
            "control event should be delivered before data events"
        );

        let mut remaining = Vec::new();
        while let Ok((event, _)) = queue.try_recv() {
            remaining.push(event);
        }
        assert!(
            remaining
                .iter()
                .any(|e| matches!(e, AudioEvent::Packet(p) if p.metadata.sequence == 4)),
            "latest packet should survive in the data lane"
        );
    }

    #[test]
    fn buffer_pressure_is_control_event() {
        let event = AudioEvent::BufferPressure {
            fill_ratio: 0.8,
            buffer_depth: 128,
        };
        assert!(
            is_control_event(&event),
            "BufferPressure must be a control event so it is never evicted"
        );
    }

    #[test]
    fn buffer_pressure_survives_full_data_lane() {
        let queue = EventQueue::new(2);

        // Fill the data lane completely.
        queue.push(AudioEvent::Packet(packet(1)));
        queue.push(AudioEvent::Packet(packet(2)));

        // Push a BufferPressure event — it should go into the control lane.
        let outcome = queue.push(AudioEvent::BufferPressure {
            fill_ratio: 1.0,
            buffer_depth: 2,
        });
        assert!(
            outcome.dropped.is_none(),
            "BufferPressure should not evict anything"
        );

        // Control events are delivered first.
        let first = queue.recv().unwrap().0;
        assert!(
            matches!(first, AudioEvent::BufferPressure { .. }),
            "BufferPressure should be delivered before data events"
        );
    }

    #[test]
    fn stream_ended_stays_behind_buffered_packets() {
        let queue = EventQueue::new(2);
        queue.push(AudioEvent::Packet(packet(1)));
        queue.push(AudioEvent::StreamEnded);

        let first = queue.recv().unwrap().0;
        let second = queue.recv().unwrap().0;

        assert!(
            matches!(first, AudioEvent::Packet(_)),
            "StreamEnded must not overtake queued packet data"
        );
        assert!(
            matches!(second, AudioEvent::StreamEnded),
            "StreamEnded should follow queued packet data"
        );
    }

    #[test]
    fn reported_len_tracks_data_lane_only() {
        let queue = EventQueue::new(2);
        let first = queue.push(AudioEvent::Packet(packet(1)));
        assert_eq!(first.len, 1);

        let control = queue.push(AudioEvent::Error(AudioError::DeviceLost));
        assert_eq!(
            control.len, 1,
            "control events should not inflate data-lane occupancy"
        );

        let (control_event, len_after_control_pop) = queue.recv().unwrap();
        assert!(matches!(control_event, AudioEvent::Error(_)));
        assert_eq!(
            len_after_control_pop, 1,
            "consuming control event should not change data-lane occupancy"
        );

        let (_, len_after_packet_pop) = queue.recv().unwrap();
        assert_eq!(len_after_packet_pop, 0);
    }
}
