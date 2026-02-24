use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Sender;
use snow_audio_recorder::AudioStreamHandle;

use crate::adapter::{AdapterCommand, StreamAdapter};
use crate::error::{Result, ScreenRecorderError};
use crate::event::{AudioCaptureEvent, RecordingEvent, StreamTimestamp};

/// Default send timeout for backpressure handling (10ms).
const SEND_TIMEOUT: Duration = Duration::from_millis(10);

/// Adapts `snow_audio_recorder::AudioStreamHandle` → `RecordingEvent::Audio`.
///
/// The forwarding thread owns the `AudioStreamHandle` and receives
/// control commands (`Pause`, `Resume`, `Stop`) via `cmd_rx`.
pub(crate) struct AudioStreamAdapter {
    cmd_tx: crossbeam_channel::Sender<AdapterCommand>,
    running: Arc<AtomicBool>,
    forward_thread: Option<std::thread::JoinHandle<Result<()>>>,
}

impl AudioStreamAdapter {
    /// Start the audio forwarding thread.
    ///
    /// `audio_handle` is moved into the forwarding thread which owns it
    /// for the lifetime of the adapter.
    /// `audio_tx` is the crossbeam sender for audio events.
    pub(crate) fn start(
        audio_handle: AudioStreamHandle,
        audio_tx: Sender<RecordingEvent>,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AdapterCommand>();
        let running = Arc::new(AtomicBool::new(true));
        let running_flag = running.clone();

        let forward_thread = std::thread::Builder::new()
            .name("snow-audio-adapter".into())
            .spawn(move || {
                let result = audio_forward_loop(audio_handle, audio_tx, cmd_rx);
                running_flag.store(false, Ordering::Release);
                result
            })
            .map_err(ScreenRecorderError::Io)?;

        Ok(Self {
            cmd_tx,
            running,
            forward_thread: Some(forward_thread),
        })
    }
}

impl StreamAdapter for AudioStreamAdapter {
    fn pause(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Pause)
            .map_err(|_| ScreenRecorderError::Encode("audio adapter command channel closed".into()))
    }

    fn resume(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Resume)
            .map_err(|_| ScreenRecorderError::Encode("audio adapter command channel closed".into()))
    }

    fn stop(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Stop)
            .map_err(|_| ScreenRecorderError::Encode("audio adapter command channel closed".into()))
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    fn join(&mut self) -> Result<()> {
        if let Some(handle) = self.forward_thread.take() {
            handle
                .join()
                .map_err(|_| ScreenRecorderError::Encode("audio adapter thread panicked".into()))?
        } else {
            Ok(())
        }
    }
}

/// The forwarding loop that bridges `AudioStreamHandle` (Mutex+Condvar) → `crossbeam_channel`.
///
/// Owns the `AudioStreamHandle` and polls it for audio events. Translates each
/// `AudioEvent` into a `RecordingEvent::Audio` and sends it on `audio_tx`.
///
/// Polls `cmd_rx` between events and after send timeouts to honor
/// pause/resume/stop commands promptly.
fn audio_forward_loop(
    audio_handle: AudioStreamHandle,
    audio_tx: Sender<RecordingEvent>,
    cmd_rx: crossbeam_channel::Receiver<AdapterCommand>,
) -> Result<()> {
    use snow_audio_recorder::{AudioEvent, RecvTimeoutError};

    loop {
        // Check for commands first (non-blocking).
        match drain_commands(&cmd_rx, &audio_handle) {
            CommandResult::Continue => {}
            CommandResult::Stop => break,
        }

        // Receive next audio event with timeout so we can check commands periodically.
        let event = match audio_handle.recv_timeout(Duration::from_millis(25)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Closed) => break,
        };

        match event {
            AudioEvent::Packet(pkt) => {
                let ts = StreamTimestamp {
                    instant: pkt
                        .metadata
                        .capture_time
                        .unwrap_or_else(std::time::Instant::now),
                    qpc_100ns: pkt.metadata.qpc_position_100ns,
                };

                let recording_event = RecordingEvent::Audio(AudioCaptureEvent::Packet {
                    source: pkt.source,
                    data: pkt.data,
                    frames: pkt.frames,
                    format: pkt.format,
                    timestamp: ts,
                });

                if send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle)
                    .is_break()
                {
                    break;
                }
            }

            AudioEvent::PacketDropped {
                source,
                dropped_frames,
            } => {
                let recording_event = RecordingEvent::Audio(AudioCaptureEvent::PacketDropped {
                    source,
                    dropped_frames,
                });
                if send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle)
                    .is_break()
                {
                    break;
                }
            }

            AudioEvent::SourceRestarted {
                source,
                old_device_id,
                new_device_id,
                downtime,
            } => {
                let recording_event = RecordingEvent::Audio(AudioCaptureEvent::SourceRestarted {
                    source,
                    old_device_id,
                    new_device_id,
                    downtime,
                });
                if send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle)
                    .is_break()
                {
                    break;
                }
            }

            AudioEvent::Paused { at } => {
                let recording_event = RecordingEvent::Audio(AudioCaptureEvent::Paused { at });
                if send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle)
                    .is_break()
                {
                    break;
                }
            }

            AudioEvent::Resumed { at, gap } => {
                let recording_event = RecordingEvent::Audio(AudioCaptureEvent::Resumed { at, gap });
                if send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle)
                    .is_break()
                {
                    break;
                }
            }

            AudioEvent::BufferPressure {
                fill_ratio,
                buffer_depth,
            } => {
                let recording_event = RecordingEvent::Audio(AudioCaptureEvent::BufferPressure {
                    fill_ratio,
                    buffer_depth,
                });
                if send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle)
                    .is_break()
                {
                    break;
                }
            }

            AudioEvent::StreamEnded => {
                let recording_event = RecordingEvent::Audio(AudioCaptureEvent::StreamEnded);
                let _ = send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle);
                break;
            }

            AudioEvent::Error(err) => {
                let recording_event =
                    RecordingEvent::Audio(AudioCaptureEvent::Error(Box::new(err)));
                let _ = send_with_backpressure(&audio_tx, recording_event, &cmd_rx, &audio_handle);
                break;
            }
        }
    }

    Ok(())
}

/// Result of draining the command channel.
enum CommandResult {
    Continue,
    Stop,
}

/// Drain all pending commands from the command channel, applying each
/// to the audio stream handle. Returns `Stop` if a stop command was received.
///
/// Note: `AudioStreamHandle::pause()`, `resume()`, and `stop()` set atomic
/// flags and do not return `Result`, so this is simpler than the video variant.
fn drain_commands(
    cmd_rx: &crossbeam_channel::Receiver<AdapterCommand>,
    audio_handle: &AudioStreamHandle,
) -> CommandResult {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            AdapterCommand::Pause => audio_handle.pause(),
            AdapterCommand::Resume => audio_handle.resume(),
            AdapterCommand::Stop => {
                audio_handle.stop();
                return CommandResult::Stop;
            }
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
fn send_with_backpressure(
    tx: &Sender<RecordingEvent>,
    event: RecordingEvent,
    cmd_rx: &crossbeam_channel::Receiver<AdapterCommand>,
    audio_handle: &AudioStreamHandle,
) -> SendOutcome {
    let mut event = event;
    loop {
        match tx.send_timeout(event, SEND_TIMEOUT) {
            Ok(()) => return SendOutcome::Sent,
            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => {
                event = returned;
                // Poll command channel during backpressure.
                match drain_commands(cmd_rx, audio_handle) {
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
