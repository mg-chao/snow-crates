use std::time::Duration;

use crossbeam_channel::{Select, TryRecvError};

use crate::adapter::RecordingAdapters;
use crate::coordinator::RecordingCoordinator;
use crate::error::Result;

/// Maximum number of audio events to drain via `try_recv` before
/// entering the `select!` wait. This gives audio priority to reduce
/// underrun risk.
const AUDIO_DRAIN_BATCH: usize = 8;

/// Timeout for the `select!` wait. When all data channels are
/// disconnected the loop exits after this timeout fires.
const SELECT_TIMEOUT: Duration = Duration::from_millis(25);

/// Run the recording event loop, processing events from all adapters
/// through the coordinator until a termination condition is met.
///
/// The loop implements:
/// - Audio-first drain policy (up to `AUDIO_DRAIN_BATCH` audio events
///   drained before each `select!` wait)
/// - Dynamic exclusion of disconnected channels from `select!`
/// - Configurable timeout for all-channels-disconnected detection
/// - Termination condition evaluation in precedence order
/// - Responsive control channel handling (Stop honored within 100ms
///   under sustained backpressure)
///
/// Returns the coordinator so the caller can call `finalize()`.
pub(crate) fn run_event_loop(
    mut coordinator: RecordingCoordinator,
    adapters: &RecordingAdapters,
) -> Result<RecordingCoordinator> {
    let mut video_disconnected = false;
    let mut audio_disconnected = false;
    let mut cursor_disconnected = false;

    loop {
        if !audio_disconnected {
            for _ in 0..AUDIO_DRAIN_BATCH {
                match adapters.audio_rx.try_recv() {
                    Ok(event) => {
                        if coordinator.handle_event(event)?.is_stop() {
                            return Ok(coordinator);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        audio_disconnected = true;
                        break;
                    }
                }
            }
        }

        if coordinator.evaluate_termination().is_stop() {
            return Ok(coordinator);
        }

        let mut sel = Select::new();

        let control_idx = sel.recv(&adapters.control_rx);

        let video_idx = if !video_disconnected {
            Some(sel.recv(&adapters.video_rx))
        } else {
            None
        };
        let audio_idx = if !audio_disconnected {
            Some(sel.recv(&adapters.audio_rx))
        } else {
            None
        };
        let cursor_idx = if !cursor_disconnected {
            Some(sel.recv(&adapters.cursor_rx))
        } else {
            None
        };

        match sel.select_timeout(SELECT_TIMEOUT) {
            Ok(oper) => {
                let idx = oper.index();

                if idx == control_idx {
                    match oper.recv(&adapters.control_rx) {
                        Ok(cmd) => {
                            match &cmd {
                                crate::event::ControlCommand::Pause => adapters.pause_all(),
                                crate::event::ControlCommand::Resume => adapters.resume_all(),
                                crate::event::ControlCommand::Stop => {}
                            }
                            if coordinator.handle_control(cmd)?.is_stop() {
                                return Ok(coordinator);
                            }
                        }
                        Err(_) => {
                            return Ok(coordinator);
                        }
                    }
                } else if Some(idx) == video_idx {
                    match oper.recv(&adapters.video_rx) {
                        Ok(event) => {
                            if coordinator.handle_event(event)?.is_stop() {
                                return Ok(coordinator);
                            }
                        }
                        Err(_) => {
                            video_disconnected = true;
                        }
                    }
                } else if Some(idx) == audio_idx {
                    match oper.recv(&adapters.audio_rx) {
                        Ok(event) => {
                            if coordinator.handle_event(event)?.is_stop() {
                                return Ok(coordinator);
                            }
                        }
                        Err(_) => {
                            audio_disconnected = true;
                        }
                    }
                } else if Some(idx) == cursor_idx {
                    match oper.recv(&adapters.cursor_rx) {
                        Ok(event) => {
                            if coordinator.handle_event(event)?.is_stop() {
                                return Ok(coordinator);
                            }
                        }
                        Err(_) => {
                            cursor_disconnected = true;
                        }
                    }
                }
            }
            Err(_timeout) => {
                if video_disconnected && audio_disconnected && cursor_disconnected {
                    return Ok(coordinator);
                }
            }
        }

        if coordinator.evaluate_termination().is_stop() {
            return Ok(coordinator);
        }
    }
}

/// Perform graceful shutdown: stop adapters, drain remaining events,
/// join forwarding threads, and process all drained events.
///
/// This function is called after `run_event_loop` returns. It ensures
/// every event that was in-flight or buffered in channels is processed
/// by the coordinator before finalization.
pub(crate) fn graceful_shutdown(
    coordinator: &mut RecordingCoordinator,
    adapters: &mut RecordingAdapters,
) -> Result<()> {
    adapters.stop_all();

    while adapters.any_running() {
        drain_all_channels(coordinator, adapters)?;
        std::thread::sleep(Duration::from_millis(1));
    }

    adapters.join_all();

    drain_all_channels(coordinator, adapters)?;

    Ok(())
}

/// Drain all per-adapter channels with audio-first priority.
/// Processes events through the coordinator. Returns after all
/// channels are empty (or disconnected).
///
/// During shutdown drain we propagate errors but ignore the
/// `EventAction` return - the purpose is to process ALL remaining
/// events regardless of termination conditions.
fn drain_all_channels(
    coordinator: &mut RecordingCoordinator,
    adapters: &RecordingAdapters,
) -> Result<()> {
    loop {
        match adapters.audio_rx.try_recv() {
            Ok(event) => {
                let _ = coordinator.handle_event(event)?;
            }
            Err(_) => break,
        }
    }

    loop {
        match adapters.video_rx.try_recv() {
            Ok(event) => {
                let _ = coordinator.handle_event(event)?;
            }
            Err(_) => break,
        }
    }

    loop {
        match adapters.cursor_rx.try_recv() {
            Ok(event) => {
                let _ = coordinator.handle_event(event)?;
            }
            Err(_) => break,
        }
    }

    loop {
        match adapters.control_rx.try_recv() {
            Ok(cmd) => {
                let _ = coordinator.handle_control(cmd)?;
            }
            Err(_) => break,
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Multiplexer-based event loop (new architecture)
// ---------------------------------------------------------------------------

use snow_core::multiplexer::{MuxCommand, MuxStatus, StreamMultiplexer};

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
                MuxStatus::SourceEnded(sid) => {
                    coordinator.mark_source_ended(sid);
                }
                MuxStatus::SourceDisconnected(sid) => {
                    coordinator.mark_source_ended(sid);
                }
                MuxStatus::SourceForwarderPanicked(sid) => {
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
/// Sends a Stop command to the multiplexer, then drains all remaining
/// output events through the coordinator.
pub(crate) fn mux_graceful_shutdown(
    coordinator: &mut RecordingCoordinator,
    multiplexer: &StreamMultiplexer<RecordingEvent>,
) -> Result<()> {
    // Send stop to all sources via multiplexer.
    let _ = multiplexer.send_command(MuxCommand::Stop);

    // Drain remaining output events.
    drain_mux_output(coordinator, multiplexer)?;

    // Drain any remaining status events.
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

    Ok(())
}

/// Drain all remaining events from the multiplexer output channel.
fn drain_mux_output(
    coordinator: &mut RecordingCoordinator,
    multiplexer: &StreamMultiplexer<RecordingEvent>,
) -> Result<()> {
    loop {
        match multiplexer.try_recv() {
            Ok(event) => {
                let _ = coordinator.handle_event(event)?;
            }
            Err(_) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RecordingVideoFormat, VideoEncodeConfig};
    use crate::coordinator::{RecordingCoordinator, VIDEO_SOURCE, AUDIO_SOURCE, CURSOR_SOURCE};
    use crate::event::{ControlCommand, RecordingEvent};
    use crate::processor::{AudioProcessor, CursorProcessor, VideoProcessor};
    use crate::timeline::PauseTimeline;
    use snow_audio_recorder::AudioEvent;
    use snow_capture::CaptureEvent;
    use snow_core::event::{SourceId, TaggedEvent};
    use snow_cursor_capture::CursorEvent;
    use std::path::PathBuf;
    use std::time::Instant;

    /// Wrap a CaptureEvent in RecordingEvent::Video.
    fn vid(event: CaptureEvent) -> RecordingEvent {
        RecordingEvent::Video(TaggedEvent { source: VIDEO_SOURCE, event })
    }
    /// Wrap an AudioEvent in RecordingEvent::Audio.
    fn aud(event: AudioEvent) -> RecordingEvent {
        RecordingEvent::Audio(TaggedEvent { source: AUDIO_SOURCE, event })
    }
    /// Wrap a CursorEvent in RecordingEvent::Cursor.
    fn cur(event: CursorEvent) -> RecordingEvent {
        RecordingEvent::Cursor(TaggedEvent { source: CURSOR_SOURCE, event })
    }

    /// Build a minimal `RecordingCoordinator` for testing.
    fn test_coordinator() -> RecordingCoordinator {
        let started_at = Instant::now();
        let timeline = PauseTimeline::new(started_at);
        let video = VideoProcessor::new(
            30,
            RecordingVideoFormat::H264Lossless,
            VideoEncodeConfig::default(),
            PathBuf::from("/tmp/test-video.h264"),
        );
        let audio = AudioProcessor::new(None, None);
        let cursor = CursorProcessor::new(0, 0);
        RecordingCoordinator::new(timeline, video, audio, cursor, 33)
    }

    /// Create a `RecordingAdapters` from pre-built channel halves.
    fn test_adapters(
        video_rx: crossbeam_channel::Receiver<RecordingEvent>,
        audio_rx: crossbeam_channel::Receiver<RecordingEvent>,
        cursor_rx: crossbeam_channel::Receiver<RecordingEvent>,
        control_rx: crossbeam_channel::Receiver<ControlCommand>,
    ) -> RecordingAdapters {
        RecordingAdapters {
            video_rx,
            audio_rx,
            cursor_rx,
            control_rx,
            video_adapter: Box::new(NoopAdapter),
            audio_adapter: None,
            cursor_adapter: None,
        }
    }

    /// A no-op adapter for testing (forwarding threads are not needed).
    struct NoopAdapter;

    impl crate::adapter::StreamAdapter for NoopAdapter {
        fn pause(&self) -> Result<()> {
            Ok(())
        }
        fn resume(&self) -> Result<()> {
            Ok(())
        }
        fn stop(&self) -> Result<()> {
            Ok(())
        }
        fn is_running(&self) -> bool {
            false
        }
        fn join(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn exits_when_all_data_channels_disconnected() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(4);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(4);
        let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();

        drop(video_tx);
        drop(audio_tx);
        drop(cursor_tx);

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let result = run_event_loop(coordinator, &adapters);
        assert!(
            result.is_ok(),
            "event loop should exit cleanly when all channels disconnect"
        );
    }

    #[test]
    fn exits_on_control_stop() {
        let (_video_tx, video_rx) = crossbeam_channel::bounded(4);
        let (_audio_tx, audio_rx) = crossbeam_channel::bounded(4);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (control_tx, control_rx) = crossbeam_channel::unbounded();

        control_tx.send(ControlCommand::Stop).unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let result = run_event_loop(coordinator, &adapters);
        assert!(result.is_ok());
    }

    #[test]
    fn exits_on_all_streams_ended() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(4);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(4);
        let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();

        video_tx
            .send(vid(CaptureEvent::StreamEnded))
            .unwrap();
        audio_tx
            .send(aud(AudioEvent::StreamEnded))
            .unwrap();
        cursor_tx
            .send(cur(CursorEvent::StreamEnded))
            .unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let result = run_event_loop(coordinator, &adapters);
        assert!(result.is_ok());
        let coord = result.unwrap();
        assert!(coord.all_streams_ended());
    }

    #[test]
    fn exits_on_fatal_video_error() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(4);
        let (_audio_tx, audio_rx) = crossbeam_channel::bounded(4);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();

        video_tx
            .send(vid(CaptureEvent::Error(
                snow_capture::error::CaptureError::BufferOverflow,
            )))
            .unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let result = run_event_loop(coordinator, &adapters);
        assert!(result.is_ok());
    }

    #[test]
    fn audio_error_is_non_fatal_loop_continues() {
        let (_video_tx, video_rx) = crossbeam_channel::bounded(4);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(4);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (control_tx, control_rx) = crossbeam_channel::unbounded();

        audio_tx
            .send(aud(AudioEvent::Error(
                snow_audio_recorder::error::AudioError::DeviceLost,
            )))
            .unwrap();

        control_tx.send(ControlCommand::Stop).unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let result = run_event_loop(coordinator, &adapters);
        assert!(result.is_ok());
        let coord = result.unwrap();
        assert!(coord.audio_ended());
    }

    #[test]
    fn audio_drain_priority_processes_audio_before_select() {
        let (_video_tx, video_rx) = crossbeam_channel::bounded(4);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(16);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (control_tx, control_rx) = crossbeam_channel::unbounded();

        audio_tx
            .send(aud(AudioEvent::StreamEnded))
            .unwrap();

        control_tx.send(ControlCommand::Stop).unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let result = run_event_loop(coordinator, &adapters);
        assert!(result.is_ok());
        let coord = result.unwrap();
        assert!(coord.audio_ended());
    }

    #[test]
    fn control_stop_honored_under_backpressure() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(8);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(8);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (control_tx, control_rx) = crossbeam_channel::unbounded();

        for _ in 0..8 {
            let _ = video_tx.try_send(vid(CaptureEvent::FrameDropped {
                sequence: 1,
            }));
        }
        for _ in 0..8 {
            let _ = audio_tx.try_send(aud(AudioEvent::BufferPressure {
                fill_ratio: 0.9,
                buffer_depth: 8,
            }));
        }

        control_tx.send(ControlCommand::Stop).unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let start = Instant::now();
        let result = run_event_loop(coordinator, &adapters);
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(
            elapsed < Duration::from_millis(100),
            "Stop should be honored within 100ms, took {:?}",
            elapsed
        );
    }


    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A controllable adapter for testing shutdown behavior.
    /// `is_running()` returns the value of the shared `running` flag.
    struct ControllableAdapter {
        running: Arc<AtomicBool>,
    }

    impl crate::adapter::StreamAdapter for ControllableAdapter {
        fn pause(&self) -> Result<()> {
            Ok(())
        }
        fn resume(&self) -> Result<()> {
            Ok(())
        }
        fn stop(&self) -> Result<()> {
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }
        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
        fn join(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn shutdown_drains_remaining_events() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(8);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(8);
        let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(8);
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();

        video_tx
            .send(vid(CaptureEvent::StreamEnded))
            .unwrap();
        audio_tx
            .send(aud(AudioEvent::StreamEnded))
            .unwrap();
        cursor_tx
            .send(cur(CursorEvent::StreamEnded))
            .unwrap();

        let mut coordinator = test_coordinator();
        let mut adapters = RecordingAdapters {
            video_rx,
            audio_rx,
            cursor_rx,
            control_rx,
            video_adapter: Box::new(NoopAdapter),
            audio_adapter: None,
            cursor_adapter: None,
        };

        let result = graceful_shutdown(&mut coordinator, &mut adapters);
        assert!(result.is_ok(), "graceful_shutdown should succeed");

        assert!(
            coordinator.capture_ended(),
            "video StreamEnded should be processed"
        );
        assert!(
            coordinator.audio_ended(),
            "audio StreamEnded should be processed"
        );
        assert!(
            coordinator.cursor_ended(),
            "cursor StreamEnded should be processed"
        );
    }

    #[test]
    fn shutdown_audio_first_during_drain() {
        let (_video_tx, video_rx) = crossbeam_channel::bounded(8);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(8);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(8);
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();

        audio_tx
            .send(aud(AudioEvent::StreamEnded))
            .unwrap();

        let mut coordinator = test_coordinator();
        let mut adapters = RecordingAdapters {
            video_rx,
            audio_rx,
            cursor_rx,
            control_rx,
            video_adapter: Box::new(NoopAdapter),
            audio_adapter: None,
            cursor_adapter: None,
        };

        let result = graceful_shutdown(&mut coordinator, &mut adapters);
        assert!(result.is_ok());

        assert!(
            coordinator.audio_ended(),
            "audio events should be drained first"
        );
    }

    #[test]
    fn shutdown_drains_while_adapters_running() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(8);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(8);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(8);
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();

        let running = Arc::new(AtomicBool::new(true));

        audio_tx
            .send(aud(AudioEvent::StreamEnded))
            .unwrap();
        video_tx
            .send(vid(CaptureEvent::StreamEnded))
            .unwrap();

        let mut coordinator = test_coordinator();
        let mut adapters = RecordingAdapters {
            video_rx,
            audio_rx,
            cursor_rx,
            control_rx,
            video_adapter: Box::new(ControllableAdapter {
                running: running.clone(),
            }),
            audio_adapter: None,
            cursor_adapter: None,
        };

        let result = graceful_shutdown(&mut coordinator, &mut adapters);
        assert!(result.is_ok());

        assert!(!running.load(Ordering::SeqCst), "adapter should be stopped");
        assert!(coordinator.audio_ended(), "audio events should be drained");
        assert!(
            coordinator.capture_ended(),
            "video events should be drained"
        );
    }


    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_event_completeness_across_shutdown(
            n_audio_extra in 0usize..=4,
            n_video_extra in 0usize..=4,
        ) {
            let (video_tx, video_rx) = crossbeam_channel::bounded(16);
            let (audio_tx, audio_rx) = crossbeam_channel::bounded(16);
            let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(16);
            let (_control_tx, control_rx) = crossbeam_channel::unbounded();

            for _ in 0..n_audio_extra {
                audio_tx
                    .send(aud(AudioEvent::BufferPressure {
                        fill_ratio: 0.7,
                        buffer_depth: 4,
                    }))
                    .unwrap();
            }
            audio_tx
                .send(aud(AudioEvent::StreamEnded))
                .unwrap();

            for i in 0..n_video_extra {
                video_tx
                    .send(vid(CaptureEvent::FrameDropped {
                        sequence: i as u64,
                    }))
                    .unwrap();
            }
            video_tx
                .send(vid(CaptureEvent::StreamEnded))
                .unwrap();

            cursor_tx
                .send(cur(CursorEvent::StreamEnded))
                .unwrap();

            drop(video_tx);
            drop(audio_tx);
            drop(cursor_tx);

            let coordinator = test_coordinator();
            let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

            let result = run_event_loop(coordinator, &adapters);
            prop_assert!(result.is_ok(), "event loop should exit cleanly");

            let mut coordinator = result.unwrap();

            let mut shutdown_adapters = RecordingAdapters {
                video_rx: adapters.video_rx.clone(),
                audio_rx: adapters.audio_rx.clone(),
                cursor_rx: adapters.cursor_rx.clone(),
                control_rx: adapters.control_rx.clone(),
                video_adapter: Box::new(NoopAdapter),
                audio_adapter: None,
                cursor_adapter: None,
            };
            let shutdown_result = graceful_shutdown(&mut coordinator, &mut shutdown_adapters);
            prop_assert!(shutdown_result.is_ok(), "graceful_shutdown should succeed");

            prop_assert!(
                coordinator.capture_ended(),
                "video StreamEnded must be processed: {} extra video events + StreamEnded were in channel",
                n_video_extra,
            );
            prop_assert!(
                coordinator.audio_ended(),
                "audio StreamEnded must be processed: {} extra audio events + StreamEnded were in channel",
                n_audio_extra,
            );
            prop_assert!(
                coordinator.cursor_ended(),
                "cursor StreamEnded must be processed",
            );
        }
    }

    proptest! {
        #[test]
        fn prop_audio_first_drain_priority(
            n_audio in 1usize..=8,
            n_video in 0usize..=4,
        ) {
            let (video_tx, video_rx) = crossbeam_channel::bounded(16);
            let (audio_tx, audio_rx) = crossbeam_channel::bounded(16);
            let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
            let (control_tx, control_rx) = crossbeam_channel::unbounded();

            for _ in 0..(n_audio - 1) {
                audio_tx
                    .send(aud(AudioEvent::BufferPressure {
                        fill_ratio: 0.8,
                        buffer_depth: 4,
                    }))
                    .unwrap();
            }
            audio_tx
                .send(aud(AudioEvent::StreamEnded))
                .unwrap();

            for i in 0..n_video {
                video_tx
                    .send(vid(CaptureEvent::FrameDropped {
                        sequence: i as u64,
                    }))
                    .unwrap();
            }

            control_tx.send(ControlCommand::Stop).unwrap();

            let coordinator = test_coordinator();
            let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

            let result = run_event_loop(coordinator, &adapters);
            prop_assert!(result.is_ok(), "event loop should exit cleanly");

            let coord = result.unwrap();
            prop_assert!(
                coord.audio_ended(),
                "audio_ended must be true: the drain phase should have \
                 processed all {} audio events (including StreamEnded) \
                 before the select! wait handled the Stop command",
                n_audio,
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 12: Stop latency under backpressure.
        #[test]
        fn prop_stop_latency_under_backpressure(
            n_video in 1usize..=8,
            n_audio in 1usize..=8,
            n_cursor in 0usize..=4,
        ) {
            let (video_tx, video_rx) = crossbeam_channel::bounded(16);
            let (audio_tx, audio_rx) = crossbeam_channel::bounded(16);
            let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(16);
            let (control_tx, control_rx) = crossbeam_channel::unbounded();

            for _ in 0..n_video {
                let _ = video_tx.try_send(vid(CaptureEvent::FrameDropped { sequence: 1 }));
            }
            for _ in 0..n_audio {
                let _ = audio_tx.try_send(aud(AudioEvent::BufferPressure {
                    fill_ratio: 0.9,
                    buffer_depth: 8,
                }));
            }
            for _ in 0..n_cursor {
                let _ = cursor_tx.try_send(cur(CursorEvent::StreamEnded));
            }

            control_tx.send(ControlCommand::Stop).unwrap();

            let coordinator = test_coordinator();
            let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

            let start = Instant::now();
            let result = run_event_loop(coordinator, &adapters);
            let elapsed = start.elapsed();

            prop_assert!(result.is_ok(), "event loop should exit cleanly");
            prop_assert!(
                elapsed < Duration::from_millis(100),
                "Stop should be honored within 100ms under backpressure, took {:?} \
                 (video={}, audio={}, cursor={})",
                elapsed, n_video, n_audio, n_cursor,
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 13: Shutdown drain ordering.
        #[test]
        fn prop_shutdown_drain_ordering(
            n_audio_extra in 0usize..=4,
            n_video_extra in 0usize..=4,
        ) {
            let (video_tx, video_rx) = crossbeam_channel::bounded(16);
            let (audio_tx, audio_rx) = crossbeam_channel::bounded(16);
            let (cursor_tx, cursor_rx) = crossbeam_channel::bounded(16);
            let (_control_tx, control_rx) = crossbeam_channel::unbounded();

            // Fill audio channel with extra events + StreamEnded.
            for _ in 0..n_audio_extra {
                audio_tx
                    .send(aud(AudioEvent::BufferPressure {
                        fill_ratio: 0.8,
                        buffer_depth: 4,
                    }))
                    .unwrap();
            }
            audio_tx.send(aud(AudioEvent::StreamEnded)).unwrap();

            // Fill video channel with extra events + StreamEnded.
            for i in 0..n_video_extra {
                video_tx
                    .send(vid(CaptureEvent::FrameDropped { sequence: i as u64 }))
                    .unwrap();
            }
            video_tx.send(vid(CaptureEvent::StreamEnded)).unwrap();

            cursor_tx.send(cur(CursorEvent::StreamEnded)).unwrap();

            drop(video_tx);
            drop(audio_tx);
            drop(cursor_tx);

            let mut coordinator = test_coordinator();
            let mut adapters = RecordingAdapters {
                video_rx,
                audio_rx,
                cursor_rx,
                control_rx,
                video_adapter: Box::new(NoopAdapter),
                audio_adapter: None,
                cursor_adapter: None,
            };

            let result = graceful_shutdown(&mut coordinator, &mut adapters);
            prop_assert!(result.is_ok(), "graceful_shutdown should succeed");

            // Audio must be drained (StreamEnded processed).
            prop_assert!(
                coordinator.audio_ended(),
                "audio StreamEnded must be processed during drain \
                 ({} extra audio events were in channel)",
                n_audio_extra,
            );
            // Video and cursor must also be drained.
            prop_assert!(
                coordinator.capture_ended(),
                "video StreamEnded must be processed during drain",
            );
            prop_assert!(
                coordinator.cursor_ended(),
                "cursor StreamEnded must be processed during drain",
            );
        }
    }
}