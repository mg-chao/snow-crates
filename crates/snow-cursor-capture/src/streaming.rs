//! Streaming handle for cursor capture: `CursorStreamHandle`.
//!
//! Runs a polling loop on a dedicated thread and delivers [`CursorEvent`]
//! through a bounded crossbeam channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel as cb;
use snow_core::timestamp::{StreamTimestamp, TickFormat};

use crate::error::CursorCaptureError;
use crate::{CursorEvent, CursorSampler};

/// Configuration for [`CursorStreamHandle`].
#[derive(Clone, Debug)]
pub struct CursorStreamConfig {
    /// How often to poll the cursor, in milliseconds.
    pub poll_interval: Duration,
    /// Bounded channel capacity for event delivery.
    pub channel_capacity: usize,
}

impl Default for CursorStreamConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(50),
            channel_capacity: 8,
        }
    }
}

/// A streaming handle that polls [`CursorSampler`] on a dedicated thread
/// and delivers [`CursorEvent`] through a bounded channel.
///
/// Implements [`snow_core::streaming::StreamHandle<CursorEvent>`].
pub struct CursorStreamHandle {
    rx: cb::Receiver<CursorEvent>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
}

impl CursorStreamHandle {
    /// Start a cursor capture polling loop on a background thread.
    ///
    /// Returns an error if the platform cursor sampler cannot be created.
    pub fn start(config: CursorStreamConfig) -> Result<Self, CursorCaptureError> {
        let sampler = CursorSampler::new()?;
        let (tx, rx) = cb::bounded(config.channel_capacity);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let pause_flag = Arc::new(AtomicBool::new(false));

        let worker_stop = Arc::clone(&stop_flag);
        let worker_pause = Arc::clone(&pause_flag);

        let join_handle = std::thread::Builder::new()
            .name("snow-cursor-stream".into())
            .spawn(move || {
                poll_loop(sampler, &config, &tx, &worker_stop, &worker_pause);
            })
            .map_err(|e| {
                CursorCaptureError::platform(format!("failed to spawn cursor stream thread: {e}"))
            })?;

        Ok(Self {
            rx,
            stop_flag,
            pause_flag,
            join_handle: Some(join_handle),
        })
    }

    /// Blocking receive.
    pub fn recv(&self) -> Result<CursorEvent, cb::RecvError> {
        self.rx.recv()
    }

    /// Non-blocking receive.
    pub fn try_recv(&self) -> Result<CursorEvent, cb::TryRecvError> {
        self.rx.try_recv()
    }

    /// Receive with timeout.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<CursorEvent, cb::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Signal the stream to stop.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
    }

    /// Signal the stream to pause.
    pub fn pause(&self) {
        self.pause_flag.store(true, Ordering::Release);
    }

    /// Signal the stream to resume.
    pub fn resume(&self) {
        self.pause_flag.store(false, Ordering::Release);
    }

    /// Whether the stream is currently paused.
    pub fn is_paused(&self) -> bool {
        self.pause_flag.load(Ordering::Acquire)
    }

    /// Whether the stream thread is still running.
    pub fn is_running(&self) -> bool {
        self.join_handle.as_ref().is_some_and(|j| !j.is_finished())
    }
}

impl Drop for CursorStreamHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

// --- StreamHandle trait implementation ---

impl snow_core::streaming::StreamHandle<CursorEvent> for CursorStreamHandle {
    type RecvError = cb::RecvError;
    type TryRecvError = cb::TryRecvError;
    type RecvTimeoutError = cb::RecvTimeoutError;

    fn recv(&self) -> Result<CursorEvent, Self::RecvError> {
        self.recv()
    }

    fn try_recv(&self) -> Result<CursorEvent, Self::TryRecvError> {
        self.try_recv()
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<CursorEvent, Self::RecvTimeoutError> {
        self.recv_timeout(timeout)
    }

    fn stop(&self) {
        self.stop()
    }

    fn pause(&self) {
        self.pause()
    }

    fn resume(&self) {
        self.resume()
    }

    fn is_paused(&self) -> bool {
        self.is_paused()
    }

    fn is_running(&self) -> bool {
        self.is_running()
    }
}

// Error conversions (crossbeam-channel → snow-core) are provided by
// snow-core::error, so no local From impls are needed here.

// --- Polling loop ---

fn poll_loop(
    mut sampler: CursorSampler,
    config: &CursorStreamConfig,
    tx: &cb::Sender<CursorEvent>,
    stop: &AtomicBool,
    pause: &AtomicBool,
) {
    let mut pause_started: Option<Instant> = None;

    loop {
        if stop.load(Ordering::Acquire) {
            let _ = tx.send(CursorEvent::StreamEnded);
            return;
        }

        if pause.load(Ordering::Acquire) {
            if pause_started.is_none() {
                let now = Instant::now();
                pause_started = Some(now);
                let _ = tx.send(CursorEvent::Paused { at: now });
            }
        } else {
            if let Some(start) = pause_started.take() {
                let now = Instant::now();
                let gap = now.duration_since(start);
                let _ = tx.send(CursorEvent::Resumed { at: now, gap });
            }

            match sampler.sample() {
                Ok(sample) => {
                    let stream_timestamp = StreamTimestamp {
                        instant: Instant::now(),
                        raw_os_ticks: None,
                        tick_format: TickFormat::RawQpc,
                    };
                    // Best-effort send: if the channel is full, drop the sample
                    // to avoid blocking the poll loop.
                    let _ = tx.try_send(CursorEvent::Sample {
                        sample,
                        stream_timestamp,
                    });
                }
                Err(e) => {
                    let _ = tx.send(CursorEvent::Error(e));
                }
            }
        }

        std::thread::sleep(config.poll_interval);
    }
}
