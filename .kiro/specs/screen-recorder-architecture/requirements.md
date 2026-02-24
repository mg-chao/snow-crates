# Requirements Document

## Introduction

This document defines the requirements for refactoring the `snow-screen-recorder` crate from a monolithic worker architecture to a trait-based composition architecture. The refactoring introduces unified event types, stream adapter traits, focused processors, a coordinating event loop using `crossbeam::select!`, decoupled cursor capture via feature flags, eliminated mouse type duplication, and decoupled `RecordingConfig` from sub-crate types. The leaf crates (`snow-capture`, `snow-audio-recorder`, `snow-cursor-capture`) remain fully independent; all new abstractions live in `snow-screen-recorder`.

## Glossary

- **RecordingCoordinator**: The central orchestration component that owns the event loop, dispatches unified events to processors, and manages the pause timeline.
- **StreamAdapter**: A trait implemented by per-subsystem adapters that bridge leaf crate streaming handles into crossbeam channels via forwarding threads.
- **VideoStreamAdapter**: The `StreamAdapter` implementation that bridges `snow_capture::StreamHandle` events into `RecordingEvent::Video` variants on a crossbeam channel.
- **AudioStreamAdapter**: The `StreamAdapter` implementation that bridges `snow_audio_recorder::AudioStreamHandle` events into `RecordingEvent::Audio` variants on a crossbeam channel.
- **CursorStreamAdapter**: The `StreamAdapter` implementation that polls `snow_cursor_capture::CursorSampler` and sends `RecordingEvent::Cursor` variants on a crossbeam channel. Only created when `snow-capture` is built without the `cursor` feature.
- **RecordingEvent**: A unified enum (`Video`, `Audio`, `Cursor`) that normalizes all subsystem events for the coordinator's event loop.
- **ControlCommand**: An enum (`Pause`, `Resume`, `Stop`) sent on a dedicated control channel, kept separate from data-plane `RecordingEvent` traffic.
- **VideoProcessor**: A focused processor responsible for video frame encoding (primary and preview), duplicate frame handling, and resolution validation.
- **AudioProcessor**: A focused processor responsible for PCM writing, silence insertion, sample format conversion, and per-source track routing.
- **CursorProcessor**: A focused processor responsible for cursor shape deduplication, coordinate translation, and frame recording into `MouseStore`.
- **PauseTimeline**: The single source of truth for active elapsed time, updated exclusively from backend-provided pause/resume timestamps.
- **AdapterCommand**: An enum (`Pause`, `Resume`, `Stop`) sent from the adapter object to its forwarding thread via a dedicated command channel.
- **MonitorSelector**: An opaque type wrapping a `stable_id` string that identifies a monitor without exposing `snow_capture::MonitorId`.
- **WindowSelector**: An opaque type wrapping a raw window handle that identifies a window without exposing `snow_capture::WindowId`.
- **RecordingRegion**: A struct defining a capture region in virtual desktop coordinates without exposing `snow_capture::CaptureRegion`.
- **StreamTimestamp**: A normalized timestamp carrying both `Instant` and optional QPC-derived 100ns-resolution value for cross-subsystem alignment.
- **EventAction**: An enum (`Continue`, `Stop`) returned by the RecordingCoordinator after processing each event.
- **WorkerOutcome**: The result produced by the RecordingCoordinator upon finalization, containing final dimensions, pause intervals, and audio recording flags.
- **TerminationCondition**: A coordinator stop reason evaluated in precedence order: `FatalVideoError`, `ControlStop`, `AllStreamsEnded`, `AllChannelsDisconnected`.

## Requirements

### Requirement 1: Unified Event Type

**User Story:** As a developer, I want all subsystem events normalized into a single enum, so that the coordinator event loop can use `crossbeam::select!` across per-adapter channels without asymmetric polling strategies.

#### Acceptance Criteria

1. THE RecordingEvent enum SHALL carry exactly three variants: `Video(VideoCaptureEvent)`, `Audio(AudioCaptureEvent)`, and `Cursor(CursorCaptureEvent)`
2. WHEN a video capture event is received from `snow-capture`, THE VideoStreamAdapter SHALL translate it into a `RecordingEvent::Video` variant preserving all payload fields
3. WHEN an audio event is received from `snow-audio-recorder`, THE AudioStreamAdapter SHALL translate it into a `RecordingEvent::Audio` variant preserving all payload fields
4. WHEN a cursor sample is obtained, THE adapter SHALL translate it into a `RecordingEvent::Cursor` variant preserving the full `CursorFrameSample`
5. THE VideoCaptureEvent SHALL include `Paused { at: Instant }` and `Resumed { at: Instant, gap: Duration }` variants that preserve the backend's original timestamps
6. THE AudioCaptureEvent SHALL include lifecycle variants (`Paused`, `Resumed`, `SourceRestarted`, `BufferPressure`) for diagnostics and adapter health telemetry
7. THE StreamTimestamp SHALL carry both an `Instant` and an optional `qpc_100ns: Option<i64>` for cross-subsystem timestamp alignment
8. THE ControlCommand enum SHALL be transmitted on a separate control channel from RecordingEvent data channels
9. THE PauseTimeline SHALL be mutated only by `VideoCaptureEvent::Paused` and `VideoCaptureEvent::Resumed`; audio lifecycle events SHALL NOT directly modify timeline state

### Requirement 2: Stream Adapter Trait and Forwarding Threads

**User Story:** As a developer, I want a common trait for subsystem stream adapters, so that the coordinator can manage lifecycle operations uniformly and mock adapters can be substituted for testing.

#### Acceptance Criteria

1. THE StreamAdapter trait SHALL define `start`, `pause`, `resume`, `stop`, `is_running`, and `join` methods
2. WHEN `start` is called for the first time, THE adapter SHALL spawn a forwarding thread that owns the leaf crate stream handle and bridges native events into a crossbeam channel
3. IF `start` is called while the adapter is already running, THEN THE adapter SHALL return an `AlreadyStarted` error (or equivalent) and SHALL NOT spawn a second forwarding thread
4. WHEN `stop` is called on a StreamAdapter, THE adapter SHALL signal the underlying stream to stop without blocking the caller
5. WHEN `join` is called on a running StreamAdapter, THE adapter SHALL block until the forwarding thread has fully exited
6. WHEN `join` is called before `start`, THE adapter SHALL return successfully without blocking
7. WHEN the forwarding thread receives an AdapterCommand via its command channel, THE forwarding thread SHALL apply the command (`Pause`, `Resume`, `Stop`) to the owned leaf crate handle
8. THE VideoStreamAdapter SHALL forward `CaptureEvent::Paused { at }` and `CaptureEvent::Resumed { at, gap }` with the backend's original timestamps preserved in the `RecordingEvent`
9. WHEN the `cursor` feature is enabled in `snow-capture`, THE VideoStreamAdapter SHALL extract embedded cursor data from `FrameMetadata::cursor` and forward it as `RecordingEvent::Cursor` on the cursor channel
10. THE StreamAdapter trait SHALL require `Send` so that adapter objects can be held across thread boundaries

### Requirement 3: Per-Adapter Channels and Select-Based Event Loop

**User Story:** As a developer, I want per-adapter crossbeam channels with `select!`-based multiplexing, so that each subsystem has independent backpressure and the coordinator can prioritize audio to prevent underruns.

#### Acceptance Criteria

1. THE recording worker SHALL create separate crossbeam channels for video, audio, cursor, and control command streams, with capacities defined by configuration constants
2. WHEN the event loop runs, THE RecordingCoordinator SHALL use `crossbeam::select!` across all per-adapter receivers and the control receiver
3. WHEN audio events are available, THE event loop SHALL drain up to `audio_drain_batch` events (default: 8) before entering the `select!` wait to reduce audio underrun risk
4. WHEN a per-adapter channel disconnects, THE event loop SHALL mark that channel as closed and exclude it from subsequent `select!` operations
5. WHEN all three data channels (video, audio, cursor) are disconnected, THE event loop SHALL exit
6. THE event loop SHALL use a configurable select timeout (default: 25ms) to detect the all-channels-disconnected condition without busy-looping
7. THE event loop SHALL evaluate stop conditions in this precedence order: `FatalVideoError`, `ControlStop`, `AllStreamsEnded`, `AllChannelsDisconnected`
8. UNDER sustained data-plane backpressure, THE event loop SHALL continue servicing the control channel and SHALL honor `ControlCommand::Stop` within 100ms

### Requirement 4: Event Processors

**User Story:** As a developer, I want the monolithic `WorkerContext` decomposed into focused processors, so that each processor has a single responsibility and can be unit-tested independently.

#### Acceptance Criteria

1. THE VideoProcessor SHALL handle frame encoding (primary and preview), duplicate frame detection, and resolution validation
2. THE AudioProcessor SHALL route audio packets to the correct track writer (system or microphone) based on the `AudioSourceKind` field
3. WHEN an audio packet with `source == System` is written successfully, THE AudioProcessor SHALL set `recorded_system` to true
4. WHEN an audio packet with `source == Microphone` is written successfully, THE AudioProcessor SHALL set `recorded_mic` to true
5. IF the corresponding audio track writer is `None` (source disabled), THEN THE AudioProcessor SHALL return `Ok(0)` without error
6. THE CursorProcessor SHALL deduplicate cursor shapes by `shape_id`, appending a `CursorShapeRecord` only when a new `shape_id` is encountered
7. THE CursorProcessor SHALL translate cursor coordinates by subtracting `(capture_origin_x, capture_origin_y)` from sample positions
8. WHEN a cursor frame is recorded, THE CursorProcessor SHALL append exactly one `CursorFrameRecord` to `mouse_store.cursor_frames` and update `last_frame`

### Requirement 5: Recording Coordinator

**User Story:** As a developer, I want a `RecordingCoordinator` that owns the pause timeline and dispatches events to processors, so that the orchestration logic is centralized and the timeline is the single source of truth for active elapsed time.

#### Acceptance Criteria

1. WHEN `handle_event` is called with a `RecordingEvent`, THE RecordingCoordinator SHALL dispatch to the correct processor based on the event variant (Video, Audio, Cursor)
2. WHEN `handle_control` is called with a `ControlCommand`, THE RecordingCoordinator SHALL handle control-plane logic separately from data-plane event routing
3. WHEN a `VideoCaptureEvent::Paused { at }` event is received, THE RecordingCoordinator SHALL update the PauseTimeline using the backend-provided `at` timestamp
4. WHEN a `VideoCaptureEvent::Resumed { at, gap }` event is received, THE RecordingCoordinator SHALL update the PauseTimeline using the backend-provided `at` timestamp and `gap`
5. WHEN an `AudioCaptureEvent::Paused` or `AudioCaptureEvent::Resumed` event is received, THE RecordingCoordinator SHALL record diagnostics state only and SHALL NOT mutate PauseTimeline
6. THE RecordingCoordinator SHALL track `capture_ended`, `audio_ended`, and `cursor_ended` flags independently
7. WHEN all three stream-ended flags are true, THE RecordingCoordinator SHALL set the `AllStreamsEnded` termination condition
8. THE RecordingCoordinator SHALL evaluate `TerminationCondition` in precedence order and SHALL return `EventAction::Stop` when the highest-priority satisfied condition is reached
9. WHEN `finalize` is called, THE RecordingCoordinator SHALL flush encoders, write the mouse store, and close PCM writers, producing a `WorkerOutcome`
10. THE RecordingCoordinator SHALL maintain monotonically non-decreasing `last_observed_ts_ms` across video and cursor events

### Requirement 6: Graceful Shutdown and Drain

**User Story:** As a developer, I want a deadlock-free shutdown sequence that drains all remaining events, so that no data is lost and bounded-channel senders do not block forever during thread join.

#### Acceptance Criteria

1. WHEN stop is requested, THE recording worker SHALL signal all adapters to stop before attempting to join forwarding threads
2. WHILE any adapter forwarding thread is still running, THE recording worker SHALL continue draining per-adapter channels with audio-first priority
3. WHEN all forwarding threads have exited, THE recording worker SHALL join all adapter threads
4. WHEN all adapter threads are joined, THE recording worker SHALL perform a final drain pass on all per-adapter channels
5. THE shutdown sequence SHALL process all drained events through the RecordingCoordinator before finalization

### Requirement 7: Mouse Type Deduplication via From Impls

**User Story:** As a developer, I want cursor type conversions derived via `From` impls instead of manual field-by-field mapping, so that the duplicated `map_cursor_composition_mode()` function and scattered conversion code are eliminated.

#### Acceptance Criteria

1. THE `CursorShapeCompositionMode` type SHALL implement `From<snow_cursor_capture::CursorCompositionMode>`
2. THE `CursorShapeRecord` type SHALL provide a `from_cursor_shape` method that converts from `snow_cursor_capture::CursorShape`
3. THE `CursorFrameRecord` type SHALL provide a `from_sample` method that converts from `snow_cursor_capture::CursorFrameSample` with timestamp and coordinate translation parameters
4. WHEN cursor type conversion is performed, THE conversion SHALL use the `From` impl or constructor methods instead of manual field-by-field mapping

### Requirement 8: Decoupled RecordingConfig Types

**User Story:** As a developer, I want `RecordingConfig` to use its own selector types instead of re-exporting sub-crate types, so that the public API does not leak `snow_capture::MonitorId`, `snow_capture::WindowId`, or `snow_capture::CaptureRegion`.

#### Acceptance Criteria

1. THE `RecordingTarget` enum SHALL use `MonitorSelector`, `WindowSelector`, and `RecordingRegion` instead of `snow_capture` types
2. THE `MonitorSelector` SHALL wrap a `stable_id` string matching the `"{adapter_luid:016x}-{output_id:016x}"` format
3. THE `WindowSelector` SHALL wrap a `raw_handle: isize` without exposing `snow_capture::WindowId`
4. THE `RecordingRegion` SHALL define `x: i32`, `y: i32`, `width: u32`, `height: u32` in virtual desktop coordinates
5. WHEN `RecordingSession::start()` is called, THE VideoStreamAdapter SHALL resolve `MonitorSelector` to `snow_capture::MonitorId` by enumerating monitors and matching by `stable_id`
6. IF a `MonitorSelector` refers to a disconnected monitor, THEN THE VideoStreamAdapter SHALL return an `InvalidConfig` error containing the unresolved `stable_id`
7. WHEN `RecordingSession::start()` is called with a `WindowSelector`, THE VideoStreamAdapter SHALL validate that `raw_handle` resolves to a live capturable window in the current desktop session
8. IF a `WindowSelector` fails validation, THEN THE VideoStreamAdapter SHALL return an `InvalidConfig` error containing the unresolved `raw_handle`
9. IF the selected window becomes unavailable during recording, THEN THE VideoStreamAdapter SHALL forward `VideoCaptureEvent::Error` and transition the video stream to ended

### Requirement 9: Cursor Capture Decoupling via Feature Flag

**User Story:** As a developer, I want `snow-capture`'s dependency on `snow-cursor-capture` to be optional via a `cursor` feature flag, so that video capture can be used independently of cursor capture.

#### Acceptance Criteria

1. THE `snow-capture` crate SHALL declare `snow-cursor-capture` as an optional dependency gated behind a `cursor` feature that defaults to enabled
2. THE `FrameMetadata` struct SHALL expose a stable `cursor: Option<CursorData>` field in both `cursor` feature states
3. WHEN the `cursor` feature is enabled, THE `cursor` field SHALL carry decoded cursor data from the capture backend
4. WHEN the `cursor` feature is disabled, THE `cursor` field SHALL always be `None`, and the `CursorData` type SHALL remain available without requiring linkage to `snow-cursor-capture`
5. THE `snow-screen-recorder` crate SHALL forward the `cursor` feature to `snow-capture` via `snow-capture/cursor`
6. WHEN the `cursor` feature is enabled, THE VideoStreamAdapter SHALL extract cursor data from video frames and no CursorStreamAdapter SHALL be created
7. WHEN the `cursor` feature is disabled, THE RecordingSession SHALL create a standalone CursorStreamAdapter that polls `CursorSampler` at the target FPS
8. THE cursor data path SHALL be exclusive: cursor data SHALL arrive through exactly one path at any time, determined at compile time by the `cursor` feature flag
9. THE workspace SHALL compile and pass targeted recorder tests in both `cursor`-enabled and `cursor`-disabled configurations

### Requirement 10: Testability via Mock Adapters

**User Story:** As a developer, I want to test the orchestration logic without real capture or audio backends, so that I can verify coordinator behavior with deterministic, synthetic event sequences.

#### Acceptance Criteria

1. THE StreamAdapter trait SHALL allow mock implementations that write directly to crossbeam channels without spawning real capture or audio sessions
2. WHEN a mock adapter is used, THE RecordingCoordinator SHALL process events identically to events from real adapters
3. THE VideoProcessor, AudioProcessor, and CursorProcessor SHALL be independently constructable and testable with synthetic data without requiring stream handles
4. THE RecordingCoordinator and processors SHALL support an injectable clock/timestamp source so tests can run with deterministic time progression
5. THE mock adapter framework SHALL support scripted sequences including ordered events, channel disconnects, and injected subsystem errors
6. THE test suite SHALL include deterministic scenarios that cover each `TerminationCondition` path (`FatalVideoError`, `ControlStop`, `AllStreamsEnded`, `AllChannelsDisconnected`)

### Requirement 11: Error Propagation and Partial Recording

**User Story:** As a developer, I want errors from individual subsystems to be propagated through the unified event system, so that the coordinator can decide whether to continue recording or stop gracefully with a partial artifact.

#### Acceptance Criteria

1. WHEN a capture backend encounters an unrecoverable error, THE VideoStreamAdapter SHALL forward a `VideoCaptureEvent::Error` through the video channel
2. WHEN an audio device disconnects and all retries fail, THE AudioStreamAdapter SHALL forward an `AudioCaptureEvent::Error` through the audio channel
3. WHEN a video error is received, THE RecordingCoordinator SHALL stop the event loop and drain remaining audio and cursor events before finalization
4. WHEN an audio error is received, THE RecordingCoordinator SHALL mark the audio stream as ended and continue recording video and cursor data
5. IF the event channel is under backpressure, THEN THE forwarding thread SHALL use `send_timeout` with a configurable timeout (default: 10ms)
6. AFTER each `send_timeout` expiration, THE forwarding thread SHALL poll its command channel before retrying data-plane sends
7. THE control command path SHALL remain independent of data-plane backpressure and SHALL honor `AdapterCommand::Stop` within 100ms
8. THE overflow behavior for each data channel SHALL be explicitly configured at startup; the default policy SHALL be `RetryUntilStop` and SHALL NOT silently drop events
9. THE forwarding layer SHALL emit per-subsystem diagnostics counters for timeout retries and dropped events (if a non-default drop policy is configured)
