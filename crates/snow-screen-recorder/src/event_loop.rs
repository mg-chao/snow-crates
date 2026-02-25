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
        // Drain up to AUDIO_DRAIN_BATCH audio events before entering
        // the select! wait. This ensures audio gets priority to reduce
        // underrun risk.
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

        // Build a dynamic Select that only includes live channels.
        let mut sel = Select::new();

        // Control channel is always included (independent of data-plane).
        let control_idx = sel.recv(&adapters.control_rx);

        // Only include data channels that haven't disconnected.
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
                            // Control channel disconnected — treat as stop.
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
                // Timeout — check if all data channels are disconnected.
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
    // 1. Signal all adapters to stop (non-blocking).
    adapters.stop_all();

    // 2. Continue draining channels with audio-first priority
    //    while any adapter forwarding thread is still running.
    //    This catches events that were in-flight when stop was signaled.
    while adapters.any_running() {
        drain_all_channels(coordinator, adapters)?;
        std::thread::sleep(Duration::from_millis(1));
    }

    // 3. Join all adapter threads (should return immediately since
    //    we waited for them to finish in step 2).
    adapters.join_all();

    // 4. Final drain pass — process any events that arrived between
    //    the last drain and the thread joins.
    drain_all_channels(coordinator, adapters)?;

    Ok(())
}

/// Drain all per-adapter channels with audio-first priority.
/// Processes events through the coordinator. Returns after all
/// channels are empty (or disconnected).
///
/// During shutdown drain we propagate errors but ignore the
/// `EventAction` return — the purpose is to process ALL remaining
/// events regardless of termination conditions.
fn drain_all_channels(
    coordinator: &mut RecordingCoordinator,
    adapters: &RecordingAdapters,
) -> Result<()> {
    // Audio-first: drain all available audio events first.
    loop {
        match adapters.audio_rx.try_recv() {
            Ok(event) => {
                let _ = coordinator.handle_event(event)?;
            }
            Err(_) => break,
        }
    }

    // Then drain video events.
    loop {
        match adapters.video_rx.try_recv() {
            Ok(event) => {
                let _ = coordinator.handle_event(event)?;
            }
            Err(_) => break,
        }
    }

    // Then drain cursor events.
    loop {
        match adapters.cursor_rx.try_recv() {
            Ok(event) => {
                let _ = coordinator.handle_event(event)?;
            }
            Err(_) => break,
        }
    }

    // Also drain control channel (though it's less critical during shutdown).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RecordingVideoFormat, VideoEncodeConfig};
    use crate::coordinator::RecordingCoordinator;
    use crate::event::{
        AudioCaptureEvent, ControlCommand, CursorCaptureEvent, RecordingEvent, VideoCaptureEvent,
    };
    use crate::processor::{AudioProcessor, CursorProcessor, VideoProcessor};
    use crate::timeline::PauseTimeline;
    use std::path::PathBuf;
    use std::time::Instant;

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

        // Drop all senders to disconnect channels.
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

        // Send a Stop command.
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

        // Send StreamEnded on all three channels.
        video_tx
            .send(RecordingEvent::Video(VideoCaptureEvent::StreamEnded))
            .unwrap();
        audio_tx
            .send(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
            .unwrap();
        cursor_tx
            .send(RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded))
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
            .send(RecordingEvent::Video(VideoCaptureEvent::Error(
                "fatal error".into(),
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

        // Audio error is non-fatal — loop should continue.
        audio_tx
            .send(RecordingEvent::Audio(AudioCaptureEvent::Error(
                "audio device lost".into(),
            )))
            .unwrap();

        // Then send a Stop to terminate the loop.
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

        // Fill audio channel with StreamEnded events — the first one
        // should be picked up in the audio-first drain phase.
        audio_tx
            .send(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
            .unwrap();

        // Also end video and cursor so the loop terminates via AllStreamsEnded.
        // Use control stop to terminate after audio drain processes the event.
        control_tx.send(ControlCommand::Stop).unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let result = run_event_loop(coordinator, &adapters);
        assert!(result.is_ok());
        let coord = result.unwrap();
        // Audio ended flag should be set from the drain phase.
        assert!(coord.audio_ended());
    }

    #[test]
    fn control_stop_honored_under_backpressure() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(8);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(8);
        let (_cursor_tx, cursor_rx) = crossbeam_channel::bounded(4);
        let (control_tx, control_rx) = crossbeam_channel::unbounded();

        // Fill video and audio channels with non-terminal events.
        for _ in 0..8 {
            let _ = video_tx.try_send(RecordingEvent::Video(VideoCaptureEvent::FrameDropped {
                sequence: 1,
            }));
        }
        for _ in 0..8 {
            let _ = audio_tx.try_send(RecordingEvent::Audio(AudioCaptureEvent::BufferPressure {
                fill_ratio: 0.9,
                buffer_depth: 8,
            }));
        }

        // Send Stop — should be honored even with data backpressure.
        control_tx.send(ControlCommand::Stop).unwrap();

        let coordinator = test_coordinator();
        let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

        let start = Instant::now();
        let result = run_event_loop(coordinator, &adapters);
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        // Stop should be honored well within 100ms.
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

        // Pre-fill channels with events that will be in-flight at shutdown.
        video_tx
            .send(RecordingEvent::Video(VideoCaptureEvent::StreamEnded))
            .unwrap();
        audio_tx
            .send(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
            .unwrap();
        cursor_tx
            .send(RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded))
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

        // All stream-ended events should have been drained and processed.
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

        // Put audio StreamEnded in the channel — audio should be drained first.
        audio_tx
            .send(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
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

        // Audio should be processed (drained first due to audio-first priority).
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

        // The video adapter starts as "running" and transitions to stopped
        // when stop() is called.
        let running = Arc::new(AtomicBool::new(true));

        // Pre-fill channels with events.
        audio_tx
            .send(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
            .unwrap();
        video_tx
            .send(RecordingEvent::Video(VideoCaptureEvent::StreamEnded))
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

        // The adapter should no longer be running.
        assert!(!running.load(Ordering::SeqCst), "adapter should be stopped");
        // Events should have been drained during the while-loop.
        assert!(coordinator.audio_ended(), "audio events should be drained");
        assert!(
            coordinator.capture_ended(),
            "video events should be drained"
        );
    }


    use proptest::prelude::*;

    //
    // Property 11: Event completeness across shutdown
    //
    // For any set of events placed in channels before shutdown, ALL
    // of them are processed by the coordinator before finalization.
    // We verify this by pre-filling channels with a random number of
    // non-terminal events plus StreamEnded on each channel, dropping
    // all senders (simulating adapter threads exiting), running the
    // event loop followed by graceful_shutdown, and asserting that
    // all three stream-ended flags are set — proving every event
    // (including the final StreamEnded) was processed.
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

            // Fill audio channel: n_audio_extra BufferPressure events + StreamEnded.
            for _ in 0..n_audio_extra {
                audio_tx
                    .send(RecordingEvent::Audio(AudioCaptureEvent::BufferPressure {
                        fill_ratio: 0.7,
                        buffer_depth: 4,
                    }))
                    .unwrap();
            }
            audio_tx
                .send(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
                .unwrap();

            // Fill video channel: n_video_extra FrameDropped events + StreamEnded.
            for i in 0..n_video_extra {
                video_tx
                    .send(RecordingEvent::Video(VideoCaptureEvent::FrameDropped {
                        sequence: i as u64,
                    }))
                    .unwrap();
            }
            video_tx
                .send(RecordingEvent::Video(VideoCaptureEvent::StreamEnded))
                .unwrap();

            // Cursor channel: just StreamEnded (CursorCaptureEvent has no
            // lightweight non-terminal variant without constructing a full
            // CursorFrameSample, and the key property is that StreamEnded
            // is always processed).
            cursor_tx
                .send(RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded))
                .unwrap();

            // Drop all senders to simulate adapter forwarding threads exiting.
            drop(video_tx);
            drop(audio_tx);
            drop(cursor_tx);

            // Run the event loop — it will process events and exit when
            // all streams end or all channels disconnect.
            let coordinator = test_coordinator();
            let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

            let result = run_event_loop(coordinator, &adapters);
            prop_assert!(result.is_ok(), "event loop should exit cleanly");

            let mut coordinator = result.unwrap();

            // Run graceful shutdown to drain any remaining events.
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

            // Assert event completeness: every StreamEnded event was
            // processed by the coordinator, proving all events in the
            // channels at stop time were handled before finalization.
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

    //
    // Property 3: Audio-first drain priority
    //
    // For any number of audio events n_audio in 1..=8 (within one
    // drain batch), if the audio channel contains n_audio-1
    // BufferPressure events followed by a StreamEnded event, and a
    // ControlCommand::Stop is queued on the control channel, the
    // event loop processes all audio events (including StreamEnded)
    // during the audio-first drain phase — proving audio gets
    // priority before the select! wait processes the Stop command.
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

            // Fill audio channel: n_audio-1 BufferPressure events + 1 StreamEnded.
            // All fit within one AUDIO_DRAIN_BATCH (8), so the drain phase
            // should process every one of them before entering select!.
            for _ in 0..(n_audio - 1) {
                audio_tx
                    .send(RecordingEvent::Audio(AudioCaptureEvent::BufferPressure {
                        fill_ratio: 0.8,
                        buffer_depth: 4,
                    }))
                    .unwrap();
            }
            audio_tx
                .send(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
                .unwrap();

            // Fill video channel with non-terminal events.
            for i in 0..n_video {
                video_tx
                    .send(RecordingEvent::Video(VideoCaptureEvent::FrameDropped {
                        sequence: i as u64,
                    }))
                    .unwrap();
            }

            // Queue Stop — the loop should process all audio events in the
            // drain phase first, then encounter Stop in the select! phase.
            control_tx.send(ControlCommand::Stop).unwrap();

            let coordinator = test_coordinator();
            let adapters = test_adapters(video_rx, audio_rx, cursor_rx, control_rx);

            let result = run_event_loop(coordinator, &adapters);
            prop_assert!(result.is_ok(), "event loop should exit cleanly");

            let coord = result.unwrap();
            // The audio drain phase processed all n_audio events including
            // StreamEnded, so audio_ended must be true.
            prop_assert!(
                coord.audio_ended(),
                "audio_ended must be true: the drain phase should have \
                 processed all {} audio events (including StreamEnded) \
                 before the select! wait handled the Stop command",
                n_audio,
            );
        }
    }
}
