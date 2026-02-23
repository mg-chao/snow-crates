use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{AudioRecorderEngine, EngineEvent};
use crate::error::{AudioError, AudioResult, RecvError, RecvTimeoutError, TryRecvError};
use crate::event_queue::EventQueue;
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

    pub fn recv(&self) -> Result<AudioEvent, RecvError> {
        let (event, len) = self.queue.recv()?;
        self.stats.buffer_fill.store(len as u64, Ordering::Release);
        Ok(event)
    }

    pub fn try_recv(&self) -> Result<AudioEvent, TryRecvError> {
        let (event, len) = self.queue.try_recv()?;
        self.stats.buffer_fill.store(len as u64, Ordering::Release);
        Ok(event)
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<AudioEvent, RecvTimeoutError> {
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

    let mut bp = BackpressureState::new(config.backpressure_threshold, config.event_buffer_depth);

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        if pause.load(Ordering::Acquire) {
            if !was_paused {
                let now = Instant::now();
                pause_started = Some(now);
                push_event_with_drop_notice(queue, stats, AudioEvent::Paused { at: now }, &mut bp);
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
            push_event_with_drop_notice(queue, stats, AudioEvent::Resumed { at: now, gap }, &mut bp);
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
                    push_event_with_drop_notice(queue, stats, event, &mut bp);
                }
            }
            Err(err) if err.is_retryable() => {
                consecutive_errors += 1;
                stats.errors_recovered.fetch_add(1, Ordering::Relaxed);
                if consecutive_errors >= config.max_consecutive_errors {
                    push_event_with_drop_notice(queue, stats, AudioEvent::Error(err.clone()), &mut bp);
                    break;
                }
                std::thread::sleep(Duration::from_millis(16));
                continue;
            }
            Err(err) => {
                push_event_with_drop_notice(queue, stats, AudioEvent::Error(err.clone()), &mut bp);
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

    push_event_with_drop_notice(queue, stats, AudioEvent::StreamEnded, &mut bp);
    queue.close();
}

/// Tracks whether we've already emitted a `BufferPressure` event for the
/// current high-pressure episode, so we don't spam the control lane.
/// Resets once the fill ratio drops back below the threshold.
struct BackpressureState {
    threshold: f64,
    buffer_depth: usize,
    /// `true` while we're above the threshold and have already emitted.
    signalled: bool,
}

impl BackpressureState {
    fn new(threshold: f64, buffer_depth: usize) -> Self {
        Self {
            threshold,
            buffer_depth,
            signalled: false,
        }
    }

    /// Check the current fill level and return a `BufferPressure` event if
    /// we just crossed the threshold upward. Returns `None` if we're below
    /// the threshold or have already signalled for this episode.
    fn check(&mut self, current_len: usize) -> Option<AudioEvent> {
        if self.buffer_depth == 0 {
            return None;
        }
        let fill_ratio = (current_len as f64 / self.buffer_depth as f64).min(1.0);
        if fill_ratio >= self.threshold {
            if !self.signalled {
                self.signalled = true;
                return Some(AudioEvent::BufferPressure {
                    fill_ratio,
                    buffer_depth: self.buffer_depth,
                });
            }
        } else {
            // Dropped back below threshold — arm for the next crossing.
            self.signalled = false;
        }
        None
    }
}

fn push_event_with_drop_notice(
    queue: &EventQueue,
    stats: &AudioStreamStats,
    event: AudioEvent,
    bp: &mut BackpressureState,
) {
    let outcome = queue.push(event);
    stats.buffer_fill.store(outcome.len as u64, Ordering::Release);

    // Check backpressure *before* handling drops — this is the proactive signal.
    if let Some(pressure_event) = bp.check(outcome.len) {
        // BufferPressure is a control event, so it goes into the unbounded lane.
        let _ = queue.push(pressure_event);
    }

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
    use crate::error::AudioResult;
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

    #[test]
    fn backpressure_state_fires_once_per_episode() {
        let mut bp = BackpressureState::new(0.5, 4);

        // Below threshold — no signal.
        assert!(bp.check(1).is_none());

        // Cross threshold — should fire.
        let event = bp.check(2);
        assert!(
            matches!(event, Some(AudioEvent::BufferPressure { fill_ratio, buffer_depth })
                if (fill_ratio - 0.5).abs() < f64::EPSILON && buffer_depth == 4),
            "should emit BufferPressure on first crossing"
        );

        // Still above — should NOT fire again.
        assert!(bp.check(3).is_none());
        assert!(bp.check(4).is_none());

        // Drop below threshold — resets.
        assert!(bp.check(1).is_none());

        // Cross again — should fire again.
        let event = bp.check(3);
        assert!(
            matches!(event, Some(AudioEvent::BufferPressure { .. })),
            "should re-arm after dropping below threshold"
        );
    }

    #[test]
    fn backpressure_disabled_at_threshold_1() {
        let mut bp = BackpressureState::new(1.0, 4);

        // Only fires when completely full.
        assert!(bp.check(3).is_none());
        assert!(bp.check(4).is_some());
        assert!(bp.check(4).is_none()); // already signalled
    }
}
