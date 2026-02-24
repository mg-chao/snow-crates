//! Deterministic termination condition tests using the mock adapter framework.
//!
//! These tests exercise each `TerminationCondition` path through the
//! full event loop + graceful shutdown pipeline, verifying that the
//! coordinator reaches the expected state.

#[cfg(test)]
mod tests {
    use crate::adapter::testing::MockAdapterBuilder;
    use crate::config::{RecordingVideoFormat, VideoEncodeConfig};
    use crate::coordinator::RecordingCoordinator;
    use crate::event::{
        AudioCaptureEvent, ControlCommand, CursorCaptureEvent, RecordingEvent,
        VideoCaptureEvent,
    };
    use crate::event_loop::{graceful_shutdown, run_event_loop};
    use crate::processor::{AudioProcessor, CursorProcessor, VideoProcessor};
    use crate::timeline::PauseTimeline;
    use std::path::PathBuf;
    use std::time::Instant;

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

    /// FatalVideoError: a video error stops the loop, remaining audio/cursor
    /// events are drained during graceful shutdown.
    #[test]
    fn fatal_video_error_stops_loop_and_drains_remaining() {
        let (mut adapters, _control_tx) = MockAdapterBuilder::new()
            .video_events(vec![RecordingEvent::Video(VideoCaptureEvent::Error(
                "capture device lost".into(),
            ))])
            .audio_events(vec![RecordingEvent::Audio(
                AudioCaptureEvent::StreamEnded,
            )])
            .cursor_events(vec![RecordingEvent::Cursor(
                CursorCaptureEvent::StreamEnded,
            )])
            .build();

        let coordinator = test_coordinator();
        let mut coordinator = run_event_loop(coordinator, &adapters)
            .expect("event loop should exit on fatal video error");

        // The video error should have triggered a stop.
        // Remaining audio/cursor events may or may not have been processed
        // during the loop — graceful_shutdown ensures they are.
        graceful_shutdown(&mut coordinator, &mut adapters)
            .expect("graceful shutdown should succeed");

        // After shutdown, all remaining events should be drained.
        assert!(
            coordinator.audio_ended(),
            "audio StreamEnded should be drained during shutdown"
        );
        assert!(
            coordinator.cursor_ended(),
            "cursor StreamEnded should be drained during shutdown"
        );
    }

    /// ControlStop: a Stop command is honored and remaining events are drained.
    #[test]
    fn control_stop_honored_and_remaining_drained() {
        let (mut adapters, control_tx) = MockAdapterBuilder::new()
            .video_events(vec![
                RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence: 1 }),
                RecordingEvent::Video(VideoCaptureEvent::StreamEnded),
            ])
            .audio_events(vec![RecordingEvent::Audio(
                AudioCaptureEvent::StreamEnded,
            )])
            .cursor_events(vec![RecordingEvent::Cursor(
                CursorCaptureEvent::StreamEnded,
            )])
            .build();

        // Send Stop command.
        control_tx.send(ControlCommand::Stop).unwrap();

        let coordinator = test_coordinator();
        let mut coordinator = run_event_loop(coordinator, &adapters)
            .expect("event loop should exit on control stop");

        graceful_shutdown(&mut coordinator, &mut adapters)
            .expect("graceful shutdown should succeed");

        // All stream-ended events should be processed after shutdown.
        assert!(coordinator.capture_ended());
        assert!(coordinator.audio_ended());
        assert!(coordinator.cursor_ended());
    }

    /// AllStreamsEnded: all three stream-ended events trigger a clean stop.
    #[test]
    fn all_streams_ended_triggers_clean_stop() {
        let (mut adapters, _control_tx) = MockAdapterBuilder::new()
            .video_events(vec![RecordingEvent::Video(
                VideoCaptureEvent::StreamEnded,
            )])
            .audio_events(vec![RecordingEvent::Audio(
                AudioCaptureEvent::StreamEnded,
            )])
            .cursor_events(vec![RecordingEvent::Cursor(
                CursorCaptureEvent::StreamEnded,
            )])
            .build();

        let coordinator = test_coordinator();
        let mut coordinator = run_event_loop(coordinator, &adapters)
            .expect("event loop should exit when all streams end");

        graceful_shutdown(&mut coordinator, &mut adapters)
            .expect("graceful shutdown should succeed");

        assert!(coordinator.all_streams_ended());
        assert!(coordinator.capture_ended());
        assert!(coordinator.audio_ended());
        assert!(coordinator.cursor_ended());
    }

    /// AllChannelsDisconnected: when all senders are dropped (channels
    /// disconnect), the event loop exits cleanly.
    #[test]
    fn all_channels_disconnected_triggers_exit() {
        // Build with empty event lists — the mock adapters will send
        // nothing, and since they drop the senders after send_all(),
        // the channels will disconnect.
        let (mut adapters, control_tx) = MockAdapterBuilder::new()
            .video_events(vec![])
            .audio_events(vec![])
            .cursor_events(vec![])
            .build();

        // Drop control sender too so all channels disconnect.
        drop(control_tx);

        let coordinator = test_coordinator();
        let mut coordinator = run_event_loop(coordinator, &adapters)
            .expect("event loop should exit when all channels disconnect");

        graceful_shutdown(&mut coordinator, &mut adapters)
            .expect("graceful shutdown should succeed");

        // No stream-ended events were sent, so flags should be false.
        assert!(!coordinator.capture_ended());
        assert!(!coordinator.audio_ended());
        assert!(!coordinator.cursor_ended());
    }

    /// Mixed scenario: audio error (non-fatal) followed by video error (fatal).
    /// The loop should continue past the audio error but stop on the video error.
    #[test]
    fn audio_error_non_fatal_then_video_error_fatal() {
        let (mut adapters, _control_tx) = MockAdapterBuilder::new()
            .video_events(vec![RecordingEvent::Video(VideoCaptureEvent::Error(
                "fatal".into(),
            ))])
            .audio_events(vec![RecordingEvent::Audio(AudioCaptureEvent::Error(
                "device lost".into(),
            ))])
            .cursor_events(vec![RecordingEvent::Cursor(
                CursorCaptureEvent::StreamEnded,
            )])
            .build();

        let coordinator = test_coordinator();
        let mut coordinator = run_event_loop(coordinator, &adapters)
            .expect("event loop should exit on fatal video error");

        graceful_shutdown(&mut coordinator, &mut adapters)
            .expect("graceful shutdown should succeed");

        // Audio error sets audio_ended (non-fatal).
        assert!(coordinator.audio_ended());
        // Cursor StreamEnded should be drained.
        assert!(coordinator.cursor_ended());
    }

    /// Events with data: video frames, audio buffer pressure, cursor samples
    /// are all processed correctly through the mock framework.
    #[test]
    fn mock_framework_processes_data_events() {
        use snow_cursor_capture::CursorFrameSample;

        let (mut adapters, _control_tx) = MockAdapterBuilder::new()
            .video_events(vec![
                RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence: 0 }),
                RecordingEvent::Video(VideoCaptureEvent::StreamEnded),
            ])
            .audio_events(vec![
                RecordingEvent::Audio(AudioCaptureEvent::BufferPressure {
                    fill_ratio: 0.8,
                    buffer_depth: 6,
                }),
                RecordingEvent::Audio(AudioCaptureEvent::StreamEnded),
            ])
            .cursor_events(vec![
                RecordingEvent::Cursor(CursorCaptureEvent::Sample(CursorFrameSample {
                    position_x: 100,
                    position_y: 200,
                    visible: true,
                    shape_id: Some(1),
                    shape: None,
                })),
                RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded),
            ])
            .build();

        let coordinator = test_coordinator();
        let mut coordinator = run_event_loop(coordinator, &adapters)
            .expect("event loop should process all events");

        graceful_shutdown(&mut coordinator, &mut adapters)
            .expect("graceful shutdown should succeed");

        assert!(coordinator.all_streams_ended());
    }
}
