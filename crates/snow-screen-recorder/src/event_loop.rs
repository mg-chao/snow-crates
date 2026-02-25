use std::time::Duration;

use snow_core::multiplexer::{MuxCommand, MuxStatus, StreamMultiplexer};

use crate::coordinator::RecordingCoordinator;
use crate::error::Result;
use crate::event::{ControlCommand, RecordingEvent};

/// Run the recording event loop using a `StreamMultiplexer`.
///
/// The multiplexer handles audio-priority drain, per-source channels,
/// and select-based multiplexing internally. This loop simply receives
/// from the multiplexer's output channel and dispatches events to the
/// coordinator.
///
/// Control commands are received on `control_rx` and forwarded to the
/// multiplexer as `MuxCommand`s. `MuxStatus` events are polled to
/// track source lifecycle.
///
/// Returns the coordinator so the caller can call `finalize()`.
pub(crate) fn run_mux_event_loop(
    mut coordinator: RecordingCoordinator,
    multiplexer: &StreamMultiplexer<RecordingEvent>,
    control_rx: &crossbeam_channel::Receiver<ControlCommand>,
) -> Result<RecordingCoordinator> {
    loop {
        // Poll control commands (non-blocking) and forward to multiplexer.
        while let Ok(cmd) = control_rx.try_recv() {
            let mux_cmd = match &cmd {
                ControlCommand::Pause => MuxCommand::Pause,
                ControlCommand::Resume => MuxCommand::Resume,
                ControlCommand::Stop => MuxCommand::Stop,
            };
            let _ = multiplexer.send_command(mux_cmd);

            if coordinator.handle_control(cmd)?.is_stop() {
                return Ok(coordinator);
            }
        }

        // Poll multiplexer status (non-blocking) for source lifecycle.
        while let Ok(status) = multiplexer.try_recv_status() {
            match status {
                MuxStatus::SourceEnded(sid)
                | MuxStatus::SourceDisconnected(sid)
                | MuxStatus::SourceForwarderPanicked(sid) => {
                    coordinator.mark_source_ended(sid);
                }
                MuxStatus::Completed => {
                    // All sources done — drain remaining output and exit.
                    drain_mux_output(&mut coordinator, multiplexer)?;
                    return Ok(coordinator);
                }
            }
        }

        if coordinator.evaluate_termination().is_stop() {
            return Ok(coordinator);
        }

        // Receive next event from multiplexer output (with timeout).
        match multiplexer.recv_timeout(Duration::from_millis(25)) {
            Ok(event) => {
                if coordinator.handle_event(event)?.is_stop() {
                    return Ok(coordinator);
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // No events available — loop back to check control/status.
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // Output channel closed — multiplexer shut down.
                return Ok(coordinator);
            }
        }

        if coordinator.evaluate_termination().is_stop() {
            return Ok(coordinator);
        }
    }
}

/// Perform graceful shutdown using the multiplexer.
///
/// Sends a Stop command, then drains all remaining output events and
/// status notifications through the coordinator.
pub(crate) fn mux_graceful_shutdown(
    coordinator: &mut RecordingCoordinator,
    multiplexer: &StreamMultiplexer<RecordingEvent>,
) -> Result<()> {
    let _ = multiplexer.send_command(MuxCommand::Stop);
    drain_mux_output(coordinator, multiplexer)?;
    drain_mux_status(coordinator, multiplexer);
    Ok(())
}

/// Drain all remaining events from the multiplexer output channel.
fn drain_mux_output(
    coordinator: &mut RecordingCoordinator,
    multiplexer: &StreamMultiplexer<RecordingEvent>,
) -> Result<()> {
    while let Ok(event) = multiplexer.try_recv() {
        coordinator.handle_event(event)?;
    }
    Ok(())
}

/// Drain remaining status notifications, marking sources as ended.
fn drain_mux_status(
    coordinator: &mut RecordingCoordinator,
    multiplexer: &StreamMultiplexer<RecordingEvent>,
) {
    while let Ok(status) = multiplexer.try_recv_status() {
        match status {
            MuxStatus::SourceEnded(sid)
            | MuxStatus::SourceDisconnected(sid)
            | MuxStatus::SourceForwarderPanicked(sid) => {
                coordinator.mark_source_ended(sid);
            }
            MuxStatus::Completed => break,
        }
    }
}
