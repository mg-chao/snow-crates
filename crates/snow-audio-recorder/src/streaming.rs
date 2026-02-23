use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{AudioRecorderEngine, EngineEvent};
use crate::error::{AudioError, AudioResult};
use crate::packet::{AudioEvent, AudioPacket};
use crate::session::AudioStreamConfig;

#[derive(Debug)]
pub struct AudioStreamStats {
    pub packets_captured: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub frames_captured: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub source_restarts: AtomicU64,
    pub errors_recovered: AtomicU64,
    pub current_packet_rate: AtomicU64,
    pub buffer_fill: AtomicU64,
}

impl Default for AudioStreamStats {
    fn default() -> Self {
        Self {
            packets_captured: AtomicU64::new(0),
            packets_dropped: AtomicU64::new(0),
            frames_captured: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            source_restarts: AtomicU64::new(0),
            errors_recovered: AtomicU64::new(0),
            current_packet_rate: AtomicU64::new(0),
            buffer_fill: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AudioStreamStatsSnapshot {
    pub packets_captured: u64,
    pub packets_dropped: u64,
    pub frames_captured: u64,
    pub frames_dropped: u64,
    pub source_restarts: u64,
    pub errors_recovered: u64,
    pub current_packet_rate: f64,
    pub buffer_fill: u64,
}

impl AudioStreamStats {
    pub fn snapshot(&self) -> AudioStreamStatsSnapshot {
        AudioStreamStatsSnapshot {
            packets_captured: self.packets_captured.load(Ordering::Relaxed),
            packets_dropped: self.packets_dropped.load(Ordering::Relaxed),
            frames_captured: self.frames_captured.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            source_restarts: self.source_restarts.load(Ordering::Relaxed),
            errors_recovered: self.errors_recovered.load(Ordering::Relaxed),
            current_packet_rate: f64::from_bits(self.current_packet_rate.load(Ordering::Relaxed)),
            buffer_fill: self.buffer_fill.load(Ordering::Relaxed),
        }
    }
}

/// Returns `true` for control-plane events that must never be silently dropped.
/// Data-plane events (`Packet`, `PacketDropped`) are bounded and droppable.
fn is_control_event(event: &AudioEvent) -> bool {
    !matches!(event, AudioEvent::Packet(_) | AudioEvent::PacketDropped { .. })
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

    /// Total number of pending events across both lanes.
    fn total_len(&self) -> usize {
        self.control_events.len() + self.data_events.len()
    }
}

struct PushOutcome {
    dropped: Option<AudioEvent>,
    len: usize,
}

struct EventQueue {
    /// Maximum capacity for the data-event lane.
    data_depth: usize,
    state: Mutex<QueueState>,
    cv: Condvar,
}

impl EventQueue {
    fn new(depth: usize) -> Self {
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

    fn push(&self, event: AudioEvent) -> PushOutcome {
        let mut guard = self.state.lock().unwrap();
        if guard.closed {
            return PushOutcome {
                dropped: Some(event),
                len: guard.total_len(),
            };
        }

        if is_control_event(&event) {
            // Control events go into the unbounded lane — never dropped.
            guard.control_events.push_back(event);
            let len = guard.total_len();
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
        let len = guard.total_len();
        self.cv.notify_one();
        PushOutcome { dropped, len }
    }

    fn recv(&self) -> Result<(AudioEvent, usize), std::sync::mpsc::RecvError> {
        let mut guard = self.state.lock().unwrap();
        loop {
            if let Some(event) = guard.pop_next() {
                return Ok((event, guard.total_len()));
            }
            if guard.closed {
                return Err(std::sync::mpsc::RecvError);
            }
            guard = self.cv.wait(guard).unwrap();
        }
    }

    fn try_recv(&self) -> Result<(AudioEvent, usize), std::sync::mpsc::TryRecvError> {
        let mut guard = self.state.lock().unwrap();
        if let Some(event) = guard.pop_next() {
            return Ok((event, guard.total_len()));
        }
        if guard.closed {
            return Err(std::sync::mpsc::TryRecvError::Disconnected);
        }
        Err(std::sync::mpsc::TryRecvError::Empty)
    }

    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(AudioEvent, usize), std::sync::mpsc::RecvTimeoutError> {
        let mut guard = self.state.lock().unwrap();
        if let Some(event) = guard.pop_next() {
            return Ok((event, guard.total_len()));
        }

        if guard.closed {
            return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
        }

        let (mut guard, wait_result) = self
            .cv
            .wait_timeout_while(guard, timeout, |state| !state.closed && state.is_empty())
            .unwrap();

        if let Some(event) = guard.pop_next() {
            return Ok((event, guard.total_len()));
        }

        if guard.closed {
            return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
        }

        if wait_result.timed_out() {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        } else {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        }
    }

    fn close(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        self.cv.notify_all();
    }

    fn drain(&self) -> Vec<AudioEvent> {
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

pub struct AudioStreamHandle {
    queue: Arc<EventQueue>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    stats: Arc<AudioStreamStats>,
    join_handle: Option<JoinHandle<()>>,
    buffer_depth: usize,
}

impl AudioStreamHandle {
    pub(crate) fn start(
        mut engine: Box<dyn AudioRecorderEngine>,
        config: AudioStreamConfig,
    ) -> AudioResult<Self> {
        let buffer_depth = config.event_buffer_depth;
        let queue = Arc::new(EventQueue::new(buffer_depth));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let pause_flag = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(AudioStreamStats::default());

        let worker_queue = Arc::clone(&queue);
        let worker_stop = Arc::clone(&stop_flag);
        let worker_pause = Arc::clone(&pause_flag);
        let worker_stats = Arc::clone(&stats);
        let worker_config = config.clone();

        let join_handle = std::thread::Builder::new()
            .name("snow-audio-stream".into())
            .spawn(move || {
                stream_loop(
                    &mut engine,
                    &worker_config,
                    &worker_queue,
                    &worker_stop,
                    &worker_pause,
                    &worker_stats,
                );
            })
            .map_err(|err| {
                AudioError::platform(anyhow::anyhow!(
                    "failed to spawn audio stream thread: {err}"
                ))
            })?;

        Ok(Self {
            queue,
            stop_flag,
            pause_flag,
            stats,
            join_handle: Some(join_handle),
            buffer_depth,
        })
    }

    pub fn recv(&self) -> Result<AudioEvent, std::sync::mpsc::RecvError> {
        let (event, len) = self.queue.recv()?;
        self.stats.buffer_fill.store(len as u64, Ordering::Release);
        Ok(event)
    }

    pub fn try_recv(&self) -> Result<AudioEvent, std::sync::mpsc::TryRecvError> {
        let (event, len) = self.queue.try_recv()?;
        self.stats.buffer_fill.store(len as u64, Ordering::Release);
        Ok(event)
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<AudioEvent, std::sync::mpsc::RecvTimeoutError> {
        let (event, len) = self.queue.recv_timeout(timeout)?;
        self.stats.buffer_fill.store(len as u64, Ordering::Release);
        Ok(event)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
    }

    pub fn pause(&self) {
        self.pause_flag.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.pause_flag.store(false, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.pause_flag.load(Ordering::Acquire)
    }

    pub fn is_running(&self) -> bool {
        self.join_handle.as_ref().is_some_and(|j| !j.is_finished())
    }

    pub fn stats(&self) -> &Arc<AudioStreamStats> {
        &self.stats
    }

    pub fn buffer_fill_percent(&self) -> f64 {
        if self.buffer_depth == 0 {
            return 0.0;
        }
        let fill = self.stats.buffer_fill.load(Ordering::Relaxed);
        (fill as f64 / self.buffer_depth as f64).min(1.0)
    }

    pub fn stop_and_drain(mut self) -> Vec<AudioEvent> {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        let drained = self.queue.drain();
        self.queue.close();
        drained
    }
}

impl Drop for AudioStreamHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
        self.queue.close();
    }
}

fn stream_loop(
    engine: &mut Box<dyn AudioRecorderEngine>,
    config: &AudioStreamConfig,
    queue: &EventQueue,
    stop: &AtomicBool,
    pause: &AtomicBool,
    stats: &AudioStreamStats,
) {
    let mut consecutive_errors = 0usize;
    let mut was_paused = false;
    let mut pause_started: Option<Instant> = None;

    let mut packet_counter: u64 = 0;
    let mut packet_epoch = Instant::now();

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        if pause.load(Ordering::Acquire) {
            if !was_paused {
                let now = Instant::now();
                pause_started = Some(now);
                push_event_with_drop_notice(queue, stats, AudioEvent::Paused { at: now });
                was_paused = true;
            }
            std::thread::sleep(Duration::from_millis(25));
            packet_counter = 0;
            packet_epoch = Instant::now();
            continue;
        }

        if was_paused {
            let now = Instant::now();
            let gap = pause_started
                .map(|started| now.saturating_duration_since(started))
                .unwrap_or(Duration::ZERO);
            push_event_with_drop_notice(queue, stats, AudioEvent::Resumed { at: now, gap });
            was_paused = false;
            pause_started = None;
        }

        match engine.poll(Duration::from_millis(100)) {
            Ok(EngineEvent::Idle) => {}
            Ok(EngineEvent::Events(events)) => {
                consecutive_errors = 0;
                for event in events {
                    match &event {
                        AudioEvent::Packet(packet) => {
                            packet_counter = packet_counter.wrapping_add(1);
                            stats.packets_captured.fetch_add(1, Ordering::Relaxed);
                            stats
                                .frames_captured
                                .fetch_add(packet.frames as u64, Ordering::Relaxed);
                        }
                        AudioEvent::SourceRestarted { .. } => {
                            stats.source_restarts.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    push_event_with_drop_notice(queue, stats, event);
                }
            }
            Err(err) if err.is_retryable() => {
                consecutive_errors += 1;
                stats.errors_recovered.fetch_add(1, Ordering::Relaxed);
                if consecutive_errors >= config.max_consecutive_errors {
                    push_event_with_drop_notice(queue, stats, AudioEvent::Error(err.clone()));
                    break;
                }
                std::thread::sleep(Duration::from_millis(16));
                continue;
            }
            Err(err) => {
                push_event_with_drop_notice(queue, stats, AudioEvent::Error(err.clone()));
                break;
            }
        }

        if packet_epoch.elapsed() >= Duration::from_secs(1) {
            let elapsed = packet_epoch.elapsed().as_secs_f64().max(0.000_001);
            let rate = packet_counter as f64 / elapsed;
            stats
                .current_packet_rate
                .store(rate.to_bits(), Ordering::Relaxed);
            packet_counter = 0;
            packet_epoch = Instant::now();
        }
    }

    push_event_with_drop_notice(queue, stats, AudioEvent::StreamEnded);
    queue.close();
}

fn push_event_with_drop_notice(queue: &EventQueue, stats: &AudioStreamStats, event: AudioEvent) {
    let outcome = queue.push(event);
    stats.buffer_fill.store(outcome.len as u64, Ordering::Release);

    if let Some(dropped) = outcome.dropped {
        if let Some((source, dropped_frames)) = dropped_packet_info(&dropped) {
            record_dropped_packet(stats, dropped_frames);
            let notice_outcome = queue.push(AudioEvent::PacketDropped {
                source,
                dropped_frames,
            });
            stats
                .buffer_fill
                .store(notice_outcome.len as u64, Ordering::Release);
            if let Some(dropped_notice_target) = notice_outcome.dropped {
                if let Some((_, secondary_dropped_frames)) =
                    dropped_packet_info(&dropped_notice_target)
                {
                    // Best effort: account for the secondary drop, but avoid recursive
                    // drop-notification storms when the queue is saturated.
                    record_dropped_packet(stats, secondary_dropped_frames);
                }
            }
        }
    }
}

fn dropped_packet_info(event: &AudioEvent) -> Option<(crate::packet::AudioSourceKind, u64)> {
    match event {
        AudioEvent::Packet(AudioPacket { source, frames, .. }) => Some((*source, *frames as u64)),
        _ => None,
    }
}

fn record_dropped_packet(stats: &AudioStreamStats, dropped_frames: u64) {
    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
    stats
        .frames_dropped
        .fetch_add(dropped_frames, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use crate::backend::EngineEvent;
    use crate::error::{AudioError, AudioResult};
    use crate::format::{AudioFormat, AudioSampleFormat};
    use crate::packet::{AudioPacket, AudioPacketMetadata, AudioSourceKind};

    struct ScriptedEngine {
        cursor: AtomicUsize,
        events: Vec<EngineEvent>,
    }

    impl ScriptedEngine {
        fn new(events: Vec<EngineEvent>) -> Self {
            Self {
                cursor: AtomicUsize::new(0),
                events,
            }
        }
    }

    impl AudioRecorderEngine for ScriptedEngine {
        fn poll(&mut self, _timeout: Duration) -> AudioResult<EngineEvent> {
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed);
            if let Some(ev) = self.events.get(idx) {
                match ev {
                    EngineEvent::Idle => Ok(EngineEvent::Idle),
                    EngineEvent::Events(events) => {
                        let cloned = events.iter().map(|e| e.clone()).collect();
                        Ok(EngineEvent::Events(cloned))
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
                Ok(EngineEvent::Idle)
            }
        }
    }

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
        // Data lane depth of 2: room for two data events at a time.
        let queue = EventQueue::new(2);
        let stats = AudioStreamStats::default();

        // Fill the data lane with two packets.
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(1)));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(2)));
        // Push a control event — it goes into the unbounded control lane.
        push_event_with_drop_notice(
            &queue,
            &stats,
            AudioEvent::Error(AudioError::DeviceLost),
        );
        // Push two more packets, evicting both originals from the data lane.
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(3)));
        push_event_with_drop_notice(&queue, &stats, AudioEvent::Packet(packet(4)));

        // Control events are delivered first, even though they were pushed
        // between data events.
        let first = queue.recv().unwrap().0;
        assert!(
            matches!(first, AudioEvent::Error(_)),
            "control event should be delivered before data events"
        );

        // Remaining events are data-plane (packets and drop notices).
        // The key invariant: the Error was never evicted.
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
    fn stop_and_drain_returns_tail_events() {
        let engine = Box::new(ScriptedEngine::new(vec![EngineEvent::Events(vec![
            AudioEvent::Packet(packet(1)),
            AudioEvent::Packet(packet(2)),
        ])]));

        let mut config = AudioStreamConfig::default();
        config.event_buffer_depth = 8;

        let handle = AudioStreamHandle::start(engine, config).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let tail = handle.stop_and_drain();

        assert!(tail
            .iter()
            .any(|event| matches!(event, AudioEvent::Packet(_))));
    }
}
