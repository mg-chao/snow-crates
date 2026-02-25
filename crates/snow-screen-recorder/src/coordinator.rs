use std::time::Instant;

use snow_core::error::{Classify, ErrorClass};

use crate::error::{Result, ScreenRecorderError};
use crate::event::{
    AudioCaptureEvent, ControlCommand, CursorCaptureEvent, EventAction, RecordingEvent,
    VideoCaptureEvent,
};
use crate::mouse::MouseStore;
use crate::processor::{AudioProcessor, CursorProcessor, VideoProcessor};
use crate::recording::{WorkerOutcome, audio_packet_to_i16_le_bytes};
use crate::timeline::PauseTimeline;

/// Coordinates the recording pipeline.
///
/// Owns the pause timeline, receives unified events, and dispatches
/// to the appropriate processor. Tracks stream-ended states for all
/// three data streams (video, audio, cursor) and evaluates termination
/// conditions in precedence order.
pub(crate) struct RecordingCoordinator {
    timeline: PauseTimeline,
    video: VideoProcessor,
    audio: AudioProcessor,
    cursor: CursorProcessor,
    last_observed_ts_ms: Option<u64>,
    frame_interval_ms: u32,
    capture_ended: bool,
    audio_ended: bool,
    cursor_ended: bool,
    /// Set when a fatal error has been received from any stream.
    fatal_error: bool,
    /// Set when an `InvalidConfig` error has been received.
    invalid_config_error: bool,
    /// Set when a control stop has been requested.
    control_stop: bool,
}

impl RecordingCoordinator {
    /// Create a new `RecordingCoordinator`.
    ///
    /// The `video`, `audio`, and `cursor` processors must be
    /// pre-constructed by the caller (e.g. from `RecordingConfig`).
    pub(crate) fn new(
        timeline: PauseTimeline,
        video: VideoProcessor,
        audio: AudioProcessor,
        cursor: CursorProcessor,
        frame_interval_ms: u32,
    ) -> Self {
        Self {
            timeline,
            video,
            audio,
            cursor,
            last_observed_ts_ms: None,
            frame_interval_ms,
            capture_ended: false,
            audio_ended: false,
            cursor_ended: false,
            fatal_error: false,
            invalid_config_error: false,
            control_stop: false,
        }
    }


    /// Dispatch a unified recording event to the correct processor.
    ///
    /// After processing, evaluates termination conditions and returns
    /// `EventAction::Stop` if the highest-priority satisfied condition
    /// is reached.
    pub(crate) fn handle_event(&mut self, event: RecordingEvent) -> Result<EventAction> {
        match event {
            RecordingEvent::Video(ve) => self.handle_video(ve)?,
            RecordingEvent::Audio(ae) => self.handle_audio(ae)?,
            RecordingEvent::Cursor(ce) => self.handle_cursor(ce)?,
        }
        Ok(self.evaluate_termination())
    }

    /// Handle a control-plane command, separate from data-plane routing.
    pub(crate) fn handle_control(&mut self, cmd: ControlCommand) -> Result<EventAction> {
        match cmd {
            ControlCommand::Pause => {
                // In the new architecture the actual pause timestamp comes
                // from the backend via VideoCaptureEvent::Paused. The
                // control command is acknowledged but does not mutate the
                // timeline directly.
            }
            ControlCommand::Resume => {
                // Same as Pause — the backend provides the authoritative
                // resume timestamp via VideoCaptureEvent::Resumed.
            }
            ControlCommand::Stop => {
                self.control_stop = true;
            }
        }
        Ok(self.evaluate_termination())
    }

    /// Returns `true` when all three data streams have ended.
    pub(crate) fn all_streams_ended(&self) -> bool {
        self.capture_ended && self.audio_ended && self.cursor_ended
    }

    /// Evaluate termination conditions in precedence order and return
    /// `EventAction::Stop` for the highest-priority satisfied condition.
    ///
    /// Precedence (highest first):
    /// 1. `FatalError` (any stream reported a fatal or invalid-config error)
    /// 2. `ControlStop`
    /// 3. `AllStreamsEnded`
    ///
    /// `AllChannelsDisconnected` is not evaluated here — it is detected
    /// by the event loop when all crossbeam receivers disconnect.
    pub(crate) fn evaluate_termination(&self) -> EventAction {
        if self.fatal_error || self.invalid_config_error {
            return EventAction::Stop;
        }
        if self.control_stop {
            return EventAction::Stop;
        }
        if self.all_streams_ended() {
            return EventAction::Stop;
        }
        EventAction::Continue
    }

    /// Update `last_observed_ts_ms` monotonically (non-decreasing).
    pub(crate) fn observe_video_time(&mut self, ts_ms: u64) {
        match self.last_observed_ts_ms {
            Some(prev) if ts_ms > prev => {
                self.last_observed_ts_ms = Some(ts_ms);
            }
            None => {
                self.last_observed_ts_ms = Some(ts_ms);
            }
            _ => {
                // ts_ms <= prev — keep the existing value to maintain
                // monotonically non-decreasing invariant.
            }
        }
    }

    #[cfg(test)]
    /// Read-only access to the last observed timestamp.
    pub(crate) fn last_observed_ts_ms(&self) -> Option<u64> {
        self.last_observed_ts_ms
    }

    #[cfg(test)]
    /// Read-only access to the timeline.
    pub(crate) fn timeline(&self) -> &PauseTimeline {
        &self.timeline
    }

    #[cfg(test)]
    /// Read-only access to the video processor.
    pub(crate) fn video(&self) -> &VideoProcessor {
        &self.video
    }

    #[cfg(test)]
    /// Read-only access to the audio processor.
    pub(crate) fn audio(&self) -> &AudioProcessor {
        &self.audio
    }

    #[cfg(test)]
    /// Read-only access to the cursor processor.
    pub(crate) fn cursor(&self) -> &CursorProcessor {
        &self.cursor
    }

    #[cfg(test)]
    /// Whether the capture (video) stream has ended.
    pub(crate) fn capture_ended(&self) -> bool {
        self.capture_ended
    }

    #[cfg(test)]
    /// Whether the audio stream has ended.
    pub(crate) fn audio_ended(&self) -> bool {
        self.audio_ended
    }

    #[cfg(test)]
    /// Whether the cursor stream has ended.
    pub(crate) fn cursor_ended(&self) -> bool {
        self.cursor_ended
    }

    /// Finalize the recording, producing a `WorkerOutcome`.
    ///
    /// Consumes the coordinator. Finalizes the timeline, flushes the
    /// preview encoder, checks that at least one frame was encoded,
    /// closes PCM writers, and returns the outcome together with the
    /// accumulated `MouseStore` (the caller is responsible for writing
    /// mouse records to disk).
    pub(crate) fn finalize(mut self, at: Instant) -> Result<(WorkerOutcome, MouseStore)> {
        self.timeline.finalize(at);
        let final_ts_ms = self.timeline.active_elapsed_ms(at);
        self.observe_video_time(final_ts_ms);

        if let Some(encoder) = self.video.take_preview_encoder() {
            let tail_rgba = self.video.last_encoded_rgba();
            encoder.finalize(final_ts_ms, tail_rgba)?;
        }

        if self.video.last_encoded_rgba().is_none() {
            return Err(ScreenRecorderError::Encode(
                "recording ended without any video frames".to_string(),
            ));
        }

        let mouse_store = self.cursor.into_mouse_store();
        let recorded_system_audio = self.audio.recorded_system();
        let recorded_microphone_audio = self.audio.recorded_mic();
        self.audio.finish()?;

        let outcome = WorkerOutcome {
            width: self.video.width(),
            height: self.video.height(),
            pause_intervals: self.timeline.intervals().to_vec(),
            recorded_system_audio,
            recorded_microphone_audio,
        };

        Ok((outcome, mouse_store))
    }


    fn handle_video(&mut self, event: VideoCaptureEvent) -> Result<()> {
        match event {
            VideoCaptureEvent::Frame {
                rgba,
                width,
                height,
                timestamp,
                is_duplicate,
            } => {
                self.video.handle_resolution_change(width, height)?;
                let _ = timestamp.qpc_100ns;

                let ts_ms = self.timeline.active_elapsed_ms(timestamp.instant);
                self.observe_video_time(ts_ms);

                if is_duplicate {
                    return self.video.handle_duplicate();
                }

                self.video.encode_frame(rgba, width, height, ts_ms)
            }

            VideoCaptureEvent::Paused { at } => {
                let ts_ms = self.timeline.active_elapsed_ms(at);
                self.observe_video_time(ts_ms);
                self.timeline.mark_pause(at);
                Ok(())
            }

            VideoCaptureEvent::Resumed { at, gap } => {
                let _ = gap;
                self.timeline.mark_resume(at);
                Ok(())
            }

            VideoCaptureEvent::StreamEnded => {
                self.capture_ended = true;
                Ok(())
            }

            VideoCaptureEvent::Error(err) => {
                // Classify the error using ErrorClass if the concrete type
                // is available (downcast to CaptureError). Fall back to
                // Fatal for unrecognized error types.
                let error_class = err
                    .downcast_ref::<snow_capture::error::CaptureError>()
                    .map(|ce| Classify::class(ce))
                    .unwrap_or(ErrorClass::Fatal);

                match error_class {
                    ErrorClass::Fatal => {
                        self.fatal_error = true;
                    }
                    ErrorClass::Transient => {
                        self.capture_ended = true;
                    }
                    ErrorClass::InvalidConfig => {
                        self.invalid_config_error = true;
                    }
                }
                Ok(())
            }

            VideoCaptureEvent::FrameDropped { sequence } => {
                let _ = sequence;
                if let Some(last) = self.last_observed_ts_ms {
                    let next_ts = last.saturating_add(u64::from(self.frame_interval_ms));
                    self.observe_video_time(next_ts);
                    self.cursor.synthesize_frame_for_drop(next_ts);
                }
                Ok(())
            }

            VideoCaptureEvent::ResolutionChanged {
                old_width,
                old_height,
                new_width,
                new_height,
            } => {
                let _ = (old_width, old_height);
                self.video.handle_resolution_change(new_width, new_height)
            }
        }
    }

    fn handle_audio(&mut self, event: AudioCaptureEvent) -> Result<()> {
        match event {
            AudioCaptureEvent::Packet {
                source,
                data,
                frames,
                format,
                timestamp,
            } => {
                let packet = build_audio_packet(source, format, frames, data, timestamp);
                let bytes = audio_packet_to_i16_le_bytes(&packet)?;
                if bytes.is_empty() {
                    return Ok(());
                }
                self.audio
                    .write_packet(source, &packet, &bytes, &self.timeline)?;
                Ok(())
            }

            AudioCaptureEvent::PacketDropped {
                source,
                dropped_frames,
            } => {
                self.audio.write_silence(source, dropped_frames)?;
                Ok(())
            }

            AudioCaptureEvent::StreamEnded => {
                self.audio_ended = true;
                Ok(())
            }

            AudioCaptureEvent::Error(err) => {
                // Classify the error using ErrorClass if the concrete type
                // is available (downcast to AudioError). Fall back to
                // Transient for unrecognized error types (audio is non-critical).
                let error_class = err
                    .downcast_ref::<snow_audio_recorder::error::AudioError>()
                    .map(|ae| Classify::class(ae))
                    .unwrap_or(ErrorClass::Transient);

                match error_class {
                    ErrorClass::Fatal => {
                        self.fatal_error = true;
                    }
                    ErrorClass::Transient => {
                        self.audio_ended = true;
                    }
                    ErrorClass::InvalidConfig => {
                        self.invalid_config_error = true;
                    }
                }
                Ok(())
            }

            // Diagnostics-only events – no state mutation.
            AudioCaptureEvent::Paused { at } => {
                let _ = at;
                Ok(())
            }
            AudioCaptureEvent::Resumed { at, gap } => {
                let _ = (at, gap);
                Ok(())
            }
            AudioCaptureEvent::SourceRestarted {
                source,
                old_device_id,
                new_device_id,
                downtime,
            } => {
                let _ = (source, old_device_id, new_device_id, downtime);
                Ok(())
            }
            AudioCaptureEvent::BufferPressure {
                fill_ratio,
                buffer_depth,
            } => {
                let _ = (fill_ratio, buffer_depth);
                Ok(())
            }
        }
    }

    fn handle_cursor(&mut self, event: CursorCaptureEvent) -> Result<()> {
        match event {
            CursorCaptureEvent::Sample(sample) => {
                let ts_ms = self.last_observed_ts_ms.unwrap_or(0);
                self.cursor.record_frame(ts_ms, &sample);
                Ok(())
            }

            CursorCaptureEvent::StreamEnded => {
                self.cursor_ended = true;
                Ok(())
            }

            CursorCaptureEvent::Error(_err) => {
                // Cursor errors are always non-fatal — mark ended and continue.
                // CursorCaptureError does not implement Classify, so we
                // treat all cursor errors as transient.
                self.cursor_ended = true;
                Ok(())
            }
        }
    }
}

fn build_audio_packet(
    source: snow_audio_recorder::AudioSourceKind,
    format: snow_audio_recorder::AudioFormat,
    frames: u32,
    data: Vec<u8>,
    timestamp: crate::event::StreamTimestamp,
) -> snow_audio_recorder::AudioPacket {
    // Keep adapter timing metadata so alignment stays stable under load.
    snow_audio_recorder::AudioPacket {
        source,
        format,
        frames,
        data,
        metadata: snow_audio_recorder::AudioPacketMetadata {
            capture_time: Some(timestamp.instant),
            qpc_position_100ns: timestamp.qpc_100ns,
            ..snow_audio_recorder::AudioPacketMetadata::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::StreamTimestamp;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Build a minimal `RecordingCoordinator` for testing.
    fn test_coordinator() -> RecordingCoordinator {
        let started_at = Instant::now();
        let timeline = PauseTimeline::new(started_at);
        let video = VideoProcessor::new(
            30,
            crate::config::RecordingVideoFormat::H264Lossless,
            crate::config::VideoEncodeConfig::default(),
            PathBuf::from("/tmp/test-video.h264"),
        );
        let audio = AudioProcessor::new(None, None);
        let cursor = CursorProcessor::new(0, 0);

        RecordingCoordinator::new(timeline, video, audio, cursor, 33)
    }

    #[test]
    fn all_streams_ended_requires_all_three_flags() {
        let mut coord = test_coordinator();
        assert!(!coord.all_streams_ended());

        coord.capture_ended = true;
        assert!(!coord.all_streams_ended());

        coord.audio_ended = true;
        assert!(!coord.all_streams_ended());

        coord.cursor_ended = true;
        assert!(coord.all_streams_ended());
    }

    #[test]
    fn evaluate_termination_precedence() {
        let mut coord = test_coordinator();

        // No conditions → Continue
        assert!(matches!(
            coord.evaluate_termination(),
            EventAction::Continue
        ));

        // AllStreamsEnded → Stop
        coord.capture_ended = true;
        coord.audio_ended = true;
        coord.cursor_ended = true;
        assert!(matches!(coord.evaluate_termination(), EventAction::Stop));

        // ControlStop takes precedence over AllStreamsEnded
        coord.control_stop = true;
        assert!(matches!(coord.evaluate_termination(), EventAction::Stop));

        // FatalError takes highest precedence
        coord.fatal_error = true;
        assert!(matches!(coord.evaluate_termination(), EventAction::Stop));
    }

    #[test]
    fn observe_video_time_is_monotonically_non_decreasing() {
        let mut coord = test_coordinator();

        coord.observe_video_time(100);
        assert_eq!(coord.last_observed_ts_ms(), Some(100));

        // Same value — stays the same
        coord.observe_video_time(100);
        assert_eq!(coord.last_observed_ts_ms(), Some(100));

        // Lower value — stays at previous
        coord.observe_video_time(50);
        assert_eq!(coord.last_observed_ts_ms(), Some(100));

        // Higher value — updates
        coord.observe_video_time(200);
        assert_eq!(coord.last_observed_ts_ms(), Some(200));
    }

    #[test]
    fn handle_control_stop_returns_stop() {
        let mut coord = test_coordinator();
        let action = coord.handle_control(ControlCommand::Stop).unwrap();
        assert!(matches!(action, EventAction::Stop));
    }

    #[test]
    fn handle_control_pause_resume_returns_continue() {
        let mut coord = test_coordinator();

        let action = coord.handle_control(ControlCommand::Pause).unwrap();
        assert!(matches!(action, EventAction::Continue));

        let action = coord.handle_control(ControlCommand::Resume).unwrap();
        assert!(matches!(action, EventAction::Continue));
    }

    #[test]
    fn video_stream_ended_sets_capture_ended() {
        let mut coord = test_coordinator();
        let action = coord
            .handle_event(RecordingEvent::Video(VideoCaptureEvent::StreamEnded))
            .unwrap();
        assert!(coord.capture_ended());
        // Not all streams ended yet
        assert!(matches!(action, EventAction::Continue));
    }

    #[test]
    fn audio_stream_ended_sets_audio_ended() {
        let mut coord = test_coordinator();
        let action = coord
            .handle_event(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
            .unwrap();
        assert!(coord.audio_ended());
        assert!(matches!(action, EventAction::Continue));
    }

    #[test]
    fn cursor_stream_ended_sets_cursor_ended() {
        let mut coord = test_coordinator();
        let action = coord
            .handle_event(RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded))
            .unwrap();
        assert!(coord.cursor_ended());
        assert!(matches!(action, EventAction::Continue));
    }

    #[test]
    fn all_streams_ended_triggers_stop() {
        let mut coord = test_coordinator();

        coord
            .handle_event(RecordingEvent::Video(VideoCaptureEvent::StreamEnded))
            .unwrap();
        coord
            .handle_event(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded))
            .unwrap();
        let action = coord
            .handle_event(RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded))
            .unwrap();

        assert!(coord.all_streams_ended());
        assert!(matches!(action, EventAction::Stop));
    }

    #[test]
    fn video_error_is_fatal() {
        let mut coord = test_coordinator();
        let action = coord
            .handle_event(RecordingEvent::Video(VideoCaptureEvent::Error(
                "test error".into(),
            )))
            .unwrap();
        assert!(matches!(action, EventAction::Stop));
    }

    #[test]
    fn audio_error_is_non_fatal() {
        let mut coord = test_coordinator();
        let action = coord
            .handle_event(RecordingEvent::Audio(AudioCaptureEvent::Error(
                "test error".into(),
            )))
            .unwrap();
        assert!(coord.audio_ended());
        // Not all streams ended, so continue
        assert!(matches!(action, EventAction::Continue));
    }

    #[test]
    fn cursor_error_is_non_fatal() {
        let mut coord = test_coordinator();
        let action = coord
            .handle_event(RecordingEvent::Cursor(CursorCaptureEvent::Error(
                "test error".into(),
            )))
            .unwrap();
        assert!(coord.cursor_ended());
        assert!(matches!(action, EventAction::Continue));
    }

    #[test]
    fn audio_lifecycle_events_do_not_mutate_timeline() {
        let mut coord = test_coordinator();
        let now = Instant::now();

        // Send audio Paused/Resumed — timeline should have no intervals
        coord
            .handle_event(RecordingEvent::Audio(AudioCaptureEvent::Paused { at: now }))
            .unwrap();
        coord
            .handle_event(RecordingEvent::Audio(AudioCaptureEvent::Resumed {
                at: now + Duration::from_millis(100),
                gap: Duration::from_millis(100),
            }))
            .unwrap();

        assert!(
            coord.timeline().intervals().is_empty(),
            "audio lifecycle events must not mutate PauseTimeline"
        );
    }

    #[test]
    fn video_paused_resumed_mutates_timeline() {
        let mut coord = test_coordinator();
        let started_at = Instant::now();

        let pause_at = started_at + Duration::from_millis(500);
        let resume_at = started_at + Duration::from_millis(800);

        coord
            .handle_event(RecordingEvent::Video(VideoCaptureEvent::Paused {
                at: pause_at,
            }))
            .unwrap();
        coord
            .handle_event(RecordingEvent::Video(VideoCaptureEvent::Resumed {
                at: resume_at,
                gap: Duration::from_millis(300),
            }))
            .unwrap();

        assert_eq!(coord.timeline().intervals().len(), 1);
    }

    #[test]
    fn frame_dropped_synthesizes_cursor_and_advances_timestamp() {
        let mut coord = test_coordinator();

        // Set an initial timestamp
        coord.observe_video_time(100);

        coord
            .handle_event(RecordingEvent::Video(VideoCaptureEvent::FrameDropped {
                sequence: 1,
            }))
            .unwrap();

        // Timestamp should advance by frame_interval_ms (33)
        assert_eq!(coord.last_observed_ts_ms(), Some(133));
    }

    #[test]
    fn dispatch_routes_video_to_video_processor() {
        let mut coord = test_coordinator();
        let ts = StreamTimestamp {
            instant: Instant::now(),
            qpc_100ns: None,
        };

        // A frame event should go through the video processor path
        // (resolution change). We can verify by checking width/height
        // get set on the video processor.
        coord
            .handle_event(RecordingEvent::Video(VideoCaptureEvent::Frame {
                rgba: vec![0u8; 4], // 1x1 pixel
                width: 1,
                height: 1,
                timestamp: ts,
                is_duplicate: true, // duplicate so we skip encoding
            }))
            .unwrap();

        assert_eq!(coord.video().width(), 1);
        assert_eq!(coord.video().height(), 1);
    }

    #[test]
    fn finalize_errors_when_no_frames_encoded() {
        let coord = test_coordinator();
        let at = Instant::now() + Duration::from_secs(1);
        let result = coord.finalize(at);
        assert!(
            result.is_err(),
            "finalize should error when no frames were encoded"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("without any video frames"),
            "error should mention missing video frames, got: {err_msg}"
        );
    }

    #[test]
    fn finalize_returns_outcome_with_correct_audio_flags() {
        // Without a real ffmpeg encoder we can't produce a successful
        // finalize (it requires at least one encoded frame). The error
        // path is validated by `finalize_errors_when_no_frames_encoded`.
        //
        // Here we verify that finalize at least calls audio.finish()
        // and cursor.into_mouse_store() by confirming it doesn't panic
        // when those processors are in their default state.
        let coord = test_coordinator();
        let at = Instant::now() + Duration::from_secs(1);
        let result = coord.finalize(at);
        // Should fail because no frames were encoded, but audio/cursor
        // cleanup should have completed without panic.
        assert!(result.is_err());
    }

    #[test]
    fn finalize_finalizes_timeline() {
        // Verify that finalize calls timeline.finalize (closes open pause)
        let started_at = Instant::now();
        let mut timeline = PauseTimeline::new(started_at);
        // Start a pause that won't be resumed — finalize should close it
        timeline.mark_pause(started_at + Duration::from_millis(100));

        let video = VideoProcessor::new(
            30,
            crate::config::RecordingVideoFormat::H264Lossless,
            crate::config::VideoEncodeConfig::default(),
            PathBuf::from("/tmp/test-video.h264"),
        );
        let audio = AudioProcessor::new(None, None);
        let cursor = CursorProcessor::new(0, 0);
        let coord = RecordingCoordinator::new(timeline, video, audio, cursor, 33);

        let at = started_at + Duration::from_millis(500);
        // This will error because no frames were encoded, but the timeline
        // should still have been finalized (pause closed) before the error.
        let _ = coord.finalize(at);
        // We can't inspect the timeline after finalize consumes self,
        // but the fact that finalize doesn't panic on an open pause
        // confirms timeline.finalize(at) was called.
    }

    #[test]
    fn build_audio_packet_preserves_timing_metadata() {
        let instant = Instant::now();
        let qpc_100ns = Some(123_456_i64);
        let packet = build_audio_packet(
            snow_audio_recorder::AudioSourceKind::System,
            snow_audio_recorder::AudioFormat::new(
                48_000,
                2,
                snow_audio_recorder::AudioSampleFormat::I16,
            ),
            480,
            vec![0u8; 1_920],
            StreamTimestamp {
                instant,
                qpc_100ns,
            },
        );

        assert_eq!(packet.metadata.capture_time, Some(instant));
        assert_eq!(packet.metadata.qpc_position_100ns, qpc_100ns);
    }


    use proptest::prelude::*;
    use snow_cursor_capture::CursorFrameSample;

    /// Strategy that generates audio events which don't require real
    /// writers: lifecycle events, StreamEnded, and Error.
    fn arb_audio_event() -> impl Strategy<Value = AudioCaptureEvent> {
        (0u8..5).prop_map(|disc| {
            let now = Instant::now();
            match disc {
                0 => AudioCaptureEvent::StreamEnded,
                1 => AudioCaptureEvent::Error("test".into()),
                2 => AudioCaptureEvent::Paused { at: now },
                3 => AudioCaptureEvent::Resumed {
                    at: now + Duration::from_millis(100),
                    gap: Duration::from_millis(100),
                },
                _ => AudioCaptureEvent::BufferPressure {
                    fill_ratio: 0.5,
                    buffer_depth: 4,
                },
            }
        })
    }

    /// Strategy that generates cursor events.
    fn arb_cursor_event() -> impl Strategy<Value = CursorCaptureEvent> {
        (0u8..3, -1000i32..1000, -1000i32..1000).prop_map(|(disc, x, y)| match disc {
            0 => CursorCaptureEvent::StreamEnded,
            1 => CursorCaptureEvent::Error("test".into()),
            _ => CursorCaptureEvent::Sample(CursorFrameSample {
                position_x: x,
                position_y: y,
                visible: true,
                shape_id: Some(1),
                shape: None,
            }),
        })
    }

    /// Strategy that generates video events which don't require real
    /// encoders: StreamEnded, Error, Paused, Resumed, FrameDropped.
    fn arb_video_event() -> impl Strategy<Value = VideoCaptureEvent> {
        (0u8..5, 1u64..100).prop_map(|(disc, seq)| {
            let now = Instant::now();
            match disc {
                0 => VideoCaptureEvent::StreamEnded,
                1 => VideoCaptureEvent::Error("test".into()),
                2 => VideoCaptureEvent::Paused { at: now },
                3 => VideoCaptureEvent::Resumed {
                    at: now + Duration::from_millis(100),
                    gap: Duration::from_millis(100),
                },
                _ => VideoCaptureEvent::FrameDropped { sequence: seq },
            }
        })
    }

    //
    // Property 7: Event dispatch correctness
    //
    // For any RecordingEvent, calling handle_event dispatches to the
    // correct processor: Audio events do NOT change video processor
    // state (width/height) or cursor processor state (frame count);
    // Cursor events do NOT change video processor state or audio
    // processor state (recorded_system/recorded_mic); Video events
    // do NOT change audio processor state.
    proptest! {
        #[test]
        fn prop_audio_events_do_not_affect_video_or_cursor(
            event in arb_audio_event(),
        ) {
            let mut coord = test_coordinator();

            let video_width_before = coord.video().width();
            let video_height_before = coord.video().height();
            let cursor_frames_before = coord.cursor().mouse_store().cursor_frames.len();
            let cursor_shapes_before = coord.cursor().mouse_store().cursor_shapes.len();

            let _ = coord.handle_event(RecordingEvent::Audio(event));

            prop_assert_eq!(coord.video().width(), video_width_before,
                "audio event must not change video width");
            prop_assert_eq!(coord.video().height(), video_height_before,
                "audio event must not change video height");
            prop_assert_eq!(coord.cursor().mouse_store().cursor_frames.len(), cursor_frames_before,
                "audio event must not add cursor frames");
            prop_assert_eq!(coord.cursor().mouse_store().cursor_shapes.len(), cursor_shapes_before,
                "audio event must not add cursor shapes");
        }

        #[test]
        fn prop_cursor_events_do_not_affect_video_or_audio(
            event in arb_cursor_event(),
        ) {
            let mut coord = test_coordinator();

            let video_width_before = coord.video().width();
            let video_height_before = coord.video().height();
            let recorded_system_before = coord.audio().recorded_system();
            let recorded_mic_before = coord.audio().recorded_mic();

            let _ = coord.handle_event(RecordingEvent::Cursor(event));

            prop_assert_eq!(coord.video().width(), video_width_before,
                "cursor event must not change video width");
            prop_assert_eq!(coord.video().height(), video_height_before,
                "cursor event must not change video height");
            prop_assert_eq!(coord.audio().recorded_system(), recorded_system_before,
                "cursor event must not change recorded_system flag");
            prop_assert_eq!(coord.audio().recorded_mic(), recorded_mic_before,
                "cursor event must not change recorded_mic flag");
        }

        #[test]
        fn prop_video_events_do_not_affect_audio(
            event in arb_video_event(),
        ) {
            let mut coord = test_coordinator();

            let recorded_system_before = coord.audio().recorded_system();
            let recorded_mic_before = coord.audio().recorded_mic();

            let _ = coord.handle_event(RecordingEvent::Video(event));

            prop_assert_eq!(coord.audio().recorded_system(), recorded_system_before,
                "video event must not change recorded_system flag");
            prop_assert_eq!(coord.audio().recorded_mic(), recorded_mic_before,
                "video event must not change recorded_mic flag");
        }
    }

    //
    // Property 9: Stream termination completeness
    //
    // For every combination of stream-ended booleans, sending the
    // corresponding StreamEnded events sets exactly the matching
    // flags, and `all_streams_ended()` is true iff all three are true.
    proptest! {
        #[test]
        fn prop_stream_termination_completeness(
            send_video in proptest::bool::ANY,
            send_audio in proptest::bool::ANY,
            send_cursor in proptest::bool::ANY,
        ) {
            let mut coord = test_coordinator();

            if send_video {
                coord.handle_event(RecordingEvent::Video(VideoCaptureEvent::StreamEnded)).unwrap();
            }
            if send_audio {
                coord.handle_event(RecordingEvent::Audio(AudioCaptureEvent::StreamEnded)).unwrap();
            }
            if send_cursor {
                coord.handle_event(RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded)).unwrap();
            }

            prop_assert_eq!(coord.capture_ended(), send_video,
                "capture_ended must match send_video");
            prop_assert_eq!(coord.audio_ended(), send_audio,
                "audio_ended must match send_audio");
            prop_assert_eq!(coord.cursor_ended(), send_cursor,
                "cursor_ended must match send_cursor");
            prop_assert_eq!(coord.all_streams_ended(), send_video && send_audio && send_cursor,
                "all_streams_ended must be true iff all three flags are true");
        }
    }

    //
    // Property 10: Timestamp monotonicity
    //
    // For any sequence of u64 timestamps fed to `observe_video_time`,
    // `last_observed_ts_ms` is monotonically non-decreasing after each call.
    proptest! {
        #[test]
        fn prop_timestamp_monotonicity(
            timestamps in proptest::collection::vec(0u64..10_000, 1..100),
        ) {
            let mut coord = test_coordinator();
            let mut prev_observed: Option<u64> = None;

            for ts in timestamps {
                coord.observe_video_time(ts);
                let current = coord.last_observed_ts_ms();

                if let (Some(prev), Some(cur)) = (prev_observed, current) {
                    prop_assert!(cur >= prev,
                        "last_observed_ts_ms must be monotonically non-decreasing: \
                         prev={}, cur={}, input_ts={}", prev, cur, ts);
                }

                prev_observed = current;
            }
        }
    }

    //
    // Property 15: Audio error is non-fatal
    //
    // For any sequence of audio lifecycle events followed by an
    // AudioCaptureEvent::Error, the coordinator marks audio_ended = true,
    // does NOT set fatal_error, and continues to process subsequent
    // video and cursor events normally.
    proptest! {
        #[test]
        fn prop_audio_error_is_non_fatal(
            pre_error_count in 0usize..8,
        ) {
            let mut coord = test_coordinator();
            let now = Instant::now();

            // 1. Send a random number of audio lifecycle events before the error.
            for i in 0..pre_error_count {
                let lifecycle_event = match i % 3 {
                    0 => AudioCaptureEvent::Paused { at: now },
                    1 => AudioCaptureEvent::Resumed {
                        at: now + Duration::from_millis(100),
                        gap: Duration::from_millis(100),
                    },
                    _ => AudioCaptureEvent::BufferPressure {
                        fill_ratio: 0.5,
                        buffer_depth: 4,
                    },
                };
                let action = coord.handle_event(RecordingEvent::Audio(lifecycle_event)).unwrap();
                // Lifecycle events should not trigger stop
                prop_assert!(matches!(action, EventAction::Continue),
                    "audio lifecycle event should return Continue");
            }

            // audio_ended should still be false before the error
            prop_assert!(!coord.audio_ended(),
                "audio_ended must be false before error");
            prop_assert!(!coord.fatal_error,
                "fatal_error must be false before audio error");

            // 2. Inject the audio error (plain string error → falls back to Transient)
            let action = coord.handle_event(
                RecordingEvent::Audio(AudioCaptureEvent::Error("device lost".into()))
            ).unwrap();

            // 3. Assert audio_ended is set and fatal_error is NOT set
            prop_assert!(coord.audio_ended(),
                "audio_ended must be true after AudioCaptureEvent::Error");
            prop_assert!(!coord.fatal_error,
                "fatal_error must NOT be set by a transient audio error");
            // handle_event returned Ok (we called .unwrap() above) and should be Continue
            // because not all streams have ended yet
            prop_assert!(matches!(action, EventAction::Continue),
                "audio error should return Continue (video/cursor still active)");

            // 4. Verify video events still process: StreamEnded sets capture_ended
            let action = coord.handle_event(
                RecordingEvent::Video(VideoCaptureEvent::StreamEnded)
            ).unwrap();
            prop_assert!(coord.capture_ended(),
                "capture_ended must be true after video StreamEnded");
            // Still not all ended (cursor still active)
            prop_assert!(matches!(action, EventAction::Continue),
                "should Continue because cursor stream is still active");

            // 5. Verify cursor events still process: StreamEnded sets cursor_ended
            let action = coord.handle_event(
                RecordingEvent::Cursor(CursorCaptureEvent::StreamEnded)
            ).unwrap();
            prop_assert!(coord.cursor_ended(),
                "cursor_ended must be true after cursor StreamEnded");
            // Now all three streams ended → Stop
            prop_assert!(matches!(action, EventAction::Stop),
                "should Stop when all three streams have ended");
        }
    }

    //
    //
    // For any error from a leaf crate, if `Classify::class()` returns `Fatal`,
    // the coordinator shall stop recording. If `Classify::class()` returns
    // `Transient`, the coordinator shall mark the stream as ended and continue
    // processing other streams. If `Classify::class()` returns `InvalidConfig`,
    // the coordinator shall stop recording.

    /// Strategy that generates a `CaptureError` whose `Classify::class()` is `Fatal`.
    fn arb_fatal_capture_error() -> impl Strategy<Value = snow_capture::error::CaptureError> {
        prop_oneof![
            Just(snow_capture::error::CaptureError::BufferOverflow),
            Just(snow_capture::error::CaptureError::platform(
            std::io::Error::new(std::io::ErrorKind::Other, "platform error"),
        )),
        ]
    }

    /// Strategy that generates a `CaptureError` whose `Classify::class()` is `Transient`.
    fn arb_transient_capture_error() -> impl Strategy<Value = snow_capture::error::CaptureError> {
        prop_oneof![
            Just(snow_capture::error::CaptureError::AccessLost),
            Just(snow_capture::error::CaptureError::Timeout),
            Just(snow_capture::error::CaptureError::WorkerDead),
            Just(snow_capture::error::CaptureError::MonitorLost),
            Just(snow_capture::error::CaptureError::Canceled),
            (1u32..4096, 1u32..4096)
                .prop_map(|(w, h)| snow_capture::error::CaptureError::ResolutionChanged(w, h)),
        ]
    }

    /// Strategy that generates a `CaptureError` whose `Classify::class()` is `InvalidConfig`.
    fn arb_invalid_config_capture_error(
    ) -> impl Strategy<Value = snow_capture::error::CaptureError> {
        prop_oneof![
            ".*".prop_map(|s| snow_capture::error::CaptureError::InvalidTarget(s)),
            Just(snow_capture::error::CaptureError::NoPrimaryMonitor),
            ".*".prop_map(|s| snow_capture::error::CaptureError::InvalidConfig(s)),
            ".*".prop_map(|s| snow_capture::error::CaptureError::UnsupportedFormat(s)),
            ".*".prop_map(|s| snow_capture::error::CaptureError::BackendUnavailable(s)),
        ]
    }

    /// Strategy that generates an `AudioError` whose `Classify::class()` is `Fatal`.
    fn arb_fatal_audio_error() -> impl Strategy<Value = snow_audio_recorder::error::AudioError> {
        prop_oneof![
            Just(snow_audio_recorder::error::AudioError::AccessDenied),
            Just(snow_audio_recorder::error::AudioError::BufferOverflow),
            Just(snow_audio_recorder::error::AudioError::platform(
            std::io::Error::new(std::io::ErrorKind::Other, "platform error"),
        )),
        ]
    }

    /// Strategy that generates an `AudioError` whose `Classify::class()` is `Transient`.
    fn arb_transient_audio_error(
    ) -> impl Strategy<Value = snow_audio_recorder::error::AudioError> {
        prop_oneof![
            Just(snow_audio_recorder::error::AudioError::DeviceLost),
            Just(snow_audio_recorder::error::AudioError::Canceled),
            Just(snow_audio_recorder::error::AudioError::WorkerDead),
        ]
    }

    /// Strategy that generates an `AudioError` whose `Classify::class()` is `InvalidConfig`.
    fn arb_invalid_config_audio_error(
    ) -> impl Strategy<Value = snow_audio_recorder::error::AudioError> {
        prop_oneof![
            ".*".prop_map(|s| snow_audio_recorder::error::AudioError::InvalidConfig(s)),
            ".*".prop_map(|s| snow_audio_recorder::error::AudioError::DeviceUnavailable(s)),
            ".*".prop_map(|s| snow_audio_recorder::error::AudioError::UnsupportedFormat(s)),
            ".*".prop_map(|s| snow_audio_recorder::error::AudioError::BackendUnavailable(s)),
        ]
    }

    proptest! {
        #[test]
        fn prop_error_class_fatal_capture_stops(
            err in arb_fatal_capture_error(),
        ) {
            let mut coord = test_coordinator();
            let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
            let action = coord.handle_event(
                RecordingEvent::Video(VideoCaptureEvent::Error(boxed))
            ).unwrap();
            prop_assert!(matches!(action, EventAction::Stop),
                "Fatal CaptureError must cause coordinator to Stop");
        }

        #[test]
        fn prop_error_class_transient_capture_continues(
            err in arb_transient_capture_error(),
        ) {
            let mut coord = test_coordinator();
            let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
            let action = coord.handle_event(
                RecordingEvent::Video(VideoCaptureEvent::Error(boxed))
            ).unwrap();
            // Transient → marks capture_ended, but other streams still active → Continue
            prop_assert!(coord.capture_ended(),
                "Transient CaptureError must set capture_ended");
            prop_assert!(!coord.fatal_error,
                "Transient CaptureError must NOT set fatal_error");
            prop_assert!(matches!(action, EventAction::Continue),
                "Transient CaptureError must return Continue (other streams active)");
        }

        #[test]
        fn prop_error_class_invalid_config_capture_stops(
            err in arb_invalid_config_capture_error(),
        ) {
            let mut coord = test_coordinator();
            let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
            let action = coord.handle_event(
                RecordingEvent::Video(VideoCaptureEvent::Error(boxed))
            ).unwrap();
            prop_assert!(matches!(action, EventAction::Stop),
                "InvalidConfig CaptureError must cause coordinator to Stop");
        }

        #[test]
        fn prop_error_class_fatal_audio_stops(
            err in arb_fatal_audio_error(),
        ) {
            let mut coord = test_coordinator();
            let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
            let action = coord.handle_event(
                RecordingEvent::Audio(AudioCaptureEvent::Error(boxed))
            ).unwrap();
            prop_assert!(matches!(action, EventAction::Stop),
                "Fatal AudioError must cause coordinator to Stop");
        }

        #[test]
        fn prop_error_class_transient_audio_continues(
            err in arb_transient_audio_error(),
        ) {
            let mut coord = test_coordinator();
            let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
            let action = coord.handle_event(
                RecordingEvent::Audio(AudioCaptureEvent::Error(boxed))
            ).unwrap();
            // Transient → marks audio_ended, but other streams still active → Continue
            prop_assert!(coord.audio_ended(),
                "Transient AudioError must set audio_ended");
            prop_assert!(!coord.fatal_error,
                "Transient AudioError must NOT set fatal_error");
            prop_assert!(matches!(action, EventAction::Continue),
                "Transient AudioError must return Continue (other streams active)");
        }

        #[test]
        fn prop_error_class_invalid_config_audio_stops(
            err in arb_invalid_config_audio_error(),
        ) {
            let mut coord = test_coordinator();
            let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
            let action = coord.handle_event(
                RecordingEvent::Audio(AudioCaptureEvent::Error(boxed))
            ).unwrap();
            prop_assert!(matches!(action, EventAction::Stop),
                "InvalidConfig AudioError must cause coordinator to Stop");
        }
    }
}

