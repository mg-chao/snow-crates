use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use snow_cursor_capture::CursorSampler;

use crate::adapter::{AdapterCommand, AdapterDiagnostics, StreamAdapter};
use crate::error::{Result, ScreenRecorderError};
use crate::event::{CursorCaptureEvent, RecordingEvent};

/// Default send timeout for backpressure handling (10ms).
const SEND_TIMEOUT: Duration = Duration::from_millis(10);

/// Adapts `snow_cursor_capture::CursorSampler` → `RecordingEvent::Cursor`.
///
/// Only created when `snow-capture` is built without the `cursor` feature.
/// Polls `CursorSampler` at the target FPS on a dedicated thread.
pub(crate) struct CursorStreamAdapter {
    cmd_tx: crossbeam_channel::Sender<AdapterCommand>,
    running: Arc<AtomicBool>,
    forward_thread: Option<std::thread::JoinHandle<Result<()>>>,
    diagnostics: Arc<AdapterDiagnostics>,
}

impl CursorStreamAdapter {
    /// Start the cursor polling thread.
    ///
    /// `sampler` is moved into the polling thread which owns it
    /// for the lifetime of the adapter.
    /// `cursor_tx` is the crossbeam sender for cursor events.
    /// `target_fps` controls the polling rate.
    pub(crate) fn start(
        sampler: CursorSampler,
        cursor_tx: Sender<RecordingEvent>,
        target_fps: u32,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AdapterCommand>();
        let running = Arc::new(AtomicBool::new(true));
        let running_flag = running.clone();
        let diagnostics = AdapterDiagnostics::new();
        let diag = diagnostics.clone();

        let forward_thread = std::thread::Builder::new()
            .name("snow-cursor-adapter".into())
            .spawn(move || {
                let result = cursor_forward_loop(sampler, cursor_tx, cmd_rx, target_fps, &diag);
                running_flag.store(false, Ordering::Release);
                result
            })
            .map_err(ScreenRecorderError::Io)?;

        Ok(Self {
            cmd_tx,
            running,
            forward_thread: Some(forward_thread),
            diagnostics,
        })
    }

    /// Returns the diagnostics counters for this adapter.
    pub(crate) fn diagnostics(&self) -> &Arc<AdapterDiagnostics> {
        &self.diagnostics
    }
}

impl StreamAdapter for CursorStreamAdapter {
    fn pause(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Pause)
            .map_err(|_| {
                ScreenRecorderError::Encode("cursor adapter command channel closed".into())
            })
    }

    fn resume(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Resume)
            .map_err(|_| {
                ScreenRecorderError::Encode("cursor adapter command channel closed".into())
            })
    }

    fn stop(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Stop)
            .map_err(|_| {
                ScreenRecorderError::Encode("cursor adapter command channel closed".into())
            })
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    fn join(&mut self) -> Result<()> {
        if let Some(handle) = self.forward_thread.take() {
            handle
                .join()
                .map_err(|_| {
                    ScreenRecorderError::Encode("cursor adapter thread panicked".into())
                })?
        } else {
            Ok(())
        }
    }
}

/// The polling loop that samples cursor state at the target FPS.
///
/// Owns the `CursorSampler` and polls it at regular intervals. Translates each
/// `CursorFrameSample` into a `RecordingEvent::Cursor` and sends it on `cursor_tx`.
///
/// Polls `cmd_rx` between samples to honor pause/resume/stop commands promptly.
/// When paused, sleeps for the poll interval without sampling.
fn cursor_forward_loop(
    mut sampler: CursorSampler,
    cursor_tx: Sender<RecordingEvent>,
    cmd_rx: crossbeam_channel::Receiver<AdapterCommand>,
    target_fps: u32,
    diagnostics: &AdapterDiagnostics,
) -> Result<()> {
    let poll_interval = Duration::from_secs(1) / target_fps;
    let mut paused = false;

    loop {
        let tick_start = Instant::now();

        // Drain commands — CursorSampler has no pause/resume API,
        // so we track paused state locally.
        match drain_commands(&cmd_rx, &mut paused) {
            CommandResult::Continue => {}
            CommandResult::Stop => break,
        }

        if paused {
            std::thread::sleep(poll_interval);
            continue;
        }

        match sampler.sample() {
            Ok(sample) => {
                let event =
                    RecordingEvent::Cursor(CursorCaptureEvent::Sample(sample));
                if send_with_backpressure(&cursor_tx, event, &cmd_rx, &mut paused, diagnostics)
                    .is_break()
                {
                    break;
                }
            }
            Err(err) => {
                let event = RecordingEvent::Cursor(CursorCaptureEvent::Error(
                    Box::new(err),
                ));
                let _ =
                    send_with_backpressure(&cursor_tx, event, &cmd_rx, &mut paused, diagnostics);
                break;
            }
        }

        // Sleep for remaining time in the poll interval.
        let elapsed = tick_start.elapsed();
        if let Some(remaining) = poll_interval.checked_sub(elapsed) {
            std::thread::sleep(remaining);
        }
    }

    // Send StreamEnded after the loop exits.
    let _ = cursor_tx.send_timeout(
        RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded),
        SEND_TIMEOUT,
    );

    Ok(())
}

/// Result of draining the command channel.
enum CommandResult {
    Continue,
    Stop,
}

/// Drain all pending commands from the command channel.
///
/// `CursorSampler` has no pause/resume API, so commands only affect
/// the local `paused` flag. Returns `Stop` if a stop command was received.
fn drain_commands(
    cmd_rx: &crossbeam_channel::Receiver<AdapterCommand>,
    paused: &mut bool,
) -> CommandResult {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            AdapterCommand::Pause => *paused = true,
            AdapterCommand::Resume => *paused = false,
            AdapterCommand::Stop => return CommandResult::Stop,
        }
    }
    CommandResult::Continue
}

/// Outcome of a backpressure-aware send.
enum SendOutcome {
    Sent,
    /// The receiver disconnected or a stop command was received.
    Break,
}

impl SendOutcome {
    fn is_break(&self) -> bool {
        matches!(self, SendOutcome::Break)
    }
}

/// Send an event with backpressure handling.
///
/// Uses `send_timeout` (10ms) and polls the command channel between
/// retries. Returns `Break` if the channel disconnected or a stop
/// command was received during backpressure.
/// Increments diagnostics counters on timeout retries and successful sends.
fn send_with_backpressure(
    tx: &Sender<RecordingEvent>,
    event: RecordingEvent,
    cmd_rx: &crossbeam_channel::Receiver<AdapterCommand>,
    paused: &mut bool,
    diagnostics: &AdapterDiagnostics,
) -> SendOutcome {
    let mut event = event;
    loop {
        match tx.send_timeout(event, SEND_TIMEOUT) {
            Ok(()) => {
                diagnostics.record_event_forwarded();
                return SendOutcome::Sent;
            }
            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => {
                diagnostics.record_timeout_retry();
                event = returned;
                // Poll command channel during backpressure.
                match drain_commands(cmd_rx, paused) {
                    CommandResult::Continue => {}
                    CommandResult::Stop => return SendOutcome::Break,
                }
            }
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                return SendOutcome::Break;
            }
        }
    }
}
