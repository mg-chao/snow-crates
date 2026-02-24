# Implementation Plan: Screen Recorder Architecture

## Overview

Refactor `snow-screen-recorder` from a monolithic worker architecture to a trait-based composition architecture with unified event types, stream adapter traits, focused processors, a coordinating event loop using `crossbeam::select!`, decoupled cursor capture via feature flags, eliminated mouse type duplication, and decoupled `RecordingConfig` from sub-crate types. Implementation follows the 6-phase migration path from the design document.

## Tasks

- [x] 1. Phase 1 — Extract processors from WorkerContext
  - [x] 1.1 Create `VideoProcessor` struct and extract video encoding logic from `WorkerContext`
    - Create `snow-screen-recorder/src/processor/video.rs`
    - Move frame encoding (primary and preview), duplicate frame detection, and resolution validation into `VideoProcessor`
    - Preserve the `preview_encoder` field from `WorkerContext`
    - Implement `encode_frame`, `handle_duplicate`, and `handle_resolution_change` methods
    - _Requirements: 4.1_

  - [x] 1.2 Create `AudioProcessor` struct and extract audio writing logic from `WorkerContext`
    - Create `snow-screen-recorder/src/processor/audio.rs`
    - Move PCM writing, silence insertion, sample format conversion, and per-source track routing into `AudioProcessor`
    - Implement `write_packet` with `AudioSourceKind`-based routing to `system_writer` or `mic_writer`
    - Return `Ok(0)` without error when the corresponding writer is `None` (source disabled)
    - Set `recorded_system`/`recorded_mic` flags on successful writes (appended > 0)
    - _Requirements: 4.2, 4.3, 4.4, 4.5_

  - [x] 1.3 Write property test for AudioProcessor track isolation
    - **Property 4: Audio track isolation and recording flags**
    - Use `proptest` to generate arbitrary `AudioSourceKind` and packet data
    - Verify routing to correct writer and flag state after writes
    - **Validates: Requirements 4.2, 4.3, 4.4**

  - [x] 1.4 Create `CursorProcessor` struct and extract cursor recording logic from `WorkerContext`
    - Create `snow-screen-recorder/src/processor/cursor.rs`
    - Move shape deduplication (`shapes_emitted: HashSet<u64>`), coordinate translation, and frame recording into `CursorProcessor`
    - Implement `record_frame` method using `CursorShapeRecord::from_cursor_shape` and `CursorFrameRecord::from_sample`
    - Maintain `shapes_emitted.len() == mouse_store.cursor_shapes.len()` invariant
    - Append exactly one `CursorFrameRecord` per call and update `last_frame`
    - _Requirements: 4.6, 4.7, 4.8_

  - [x] 1.5 Write property test for cursor shape deduplication invariant
    - **Property 5: Cursor shape deduplication invariant**
    - Use `proptest` to generate sequences of cursor samples with varying `shape_id` values
    - Assert `shapes_emitted.len() == mouse_store.cursor_shapes.len()` after each call
    - Assert every `shape_id` referenced by `CursorFrameRecord` exists in `cursor_shapes`
    - **Validates: Requirement 4.6**

  - [x] 1.6 Write property test for cursor frame coordinate translation
    - **Property 6: Cursor frame recording with coordinate translation**
    - Use `proptest` to generate arbitrary `(position_x, position_y)` and `(origin_x, origin_y)` values
    - Assert appended record has coordinates `(position_x - origin_x, position_y - origin_y)`
    - Assert `last_frame` equals the appended record
    - **Validates: Requirements 4.7, 4.8**

  - [x] 1.7 Create `processor` module and wire processors into existing `WorkerContext`
    - Create `snow-screen-recorder/src/processor/mod.rs` re-exporting `VideoProcessor`, `AudioProcessor`, `CursorProcessor`
    - Refactor `WorkerContext` to delegate to the new processors instead of inline logic
    - Ensure existing tests still pass with the delegated implementation
    - _Requirements: 4.1, 4.2, 4.6_

- [x] 2. Checkpoint — Phase 1 complete
  - Ensure all tests pass, ask the user if questions arise.

- [x] 3. Phase 2 — Unified event types and RecordingCoordinator
  - [x] 3.1 Define `RecordingEvent`, `VideoCaptureEvent`, `AudioCaptureEvent`, `CursorCaptureEvent` enums
    - Create `snow-screen-recorder/src/event.rs`
    - `RecordingEvent` with exactly three variants: `Video(VideoCaptureEvent)`, `Audio(AudioCaptureEvent)`, `Cursor(CursorCaptureEvent)`
    - `VideoCaptureEvent` must include `Paused { at: Instant }` and `Resumed { at: Instant, gap: Duration }` preserving backend timestamps
    - `AudioCaptureEvent` must include lifecycle variants: `Paused`, `Resumed`, `SourceRestarted`, `BufferPressure`
    - All error variants use `Box<dyn std::error::Error + Send + Sync>`
    - _Requirements: 1.1, 1.5, 1.6_

  - [x] 3.2 Define `StreamTimestamp`, `ControlCommand`, `EventAction`, and `TerminationCondition` types
    - `StreamTimestamp` with `instant: Instant` and `qpc_100ns: Option<i64>`
    - `ControlCommand` enum with `Pause`, `Resume`, `Stop` — transmitted on separate control channel
    - `EventAction` enum with `Continue`, `Stop`
    - `TerminationCondition` with precedence order: `FatalVideoError`, `ControlStop`, `AllStreamsEnded`, `AllChannelsDisconnected`
    - _Requirements: 1.7, 1.8, 3.7_

  - [x] 3.3 Implement `PauseTimeline` with backend-timestamp-only mutation
    - Ensure `PauseTimeline` is mutated only by `VideoCaptureEvent::Paused` and `VideoCaptureEvent::Resumed`
    - Audio lifecycle events must NOT directly modify timeline state
    - _Requirements: 1.9, 5.3, 5.4_

  - [x] 3.4 Write property test for pause timeline using backend timestamps
    - **Property 8: Pause timeline uses backend timestamps**
    - Use `proptest` to generate sequences of `Paused { at }` and `Resumed { at, gap }` events
    - Assert the timeline records the backend-provided `at` value, not `Instant::now()`
    - **Validates: Requirements 5.3, 5.4**

  - [x] 3.5 Implement `RecordingCoordinator` with event dispatch and stream-ended tracking
    - Create `snow-screen-recorder/src/coordinator.rs`
    - Implement `handle_event` dispatching `Video` → `VideoProcessor`, `Audio` → `AudioProcessor`, `Cursor` → `CursorProcessor`
    - Implement `handle_control` for control-plane logic separate from data-plane routing
    - Track `capture_ended`, `audio_ended`, `cursor_ended` flags independently
    - Implement `all_streams_ended()` returning true iff all three flags are true
    - Evaluate `TerminationCondition` in precedence order, return `EventAction::Stop` for highest-priority satisfied condition
    - Maintain monotonically non-decreasing `last_observed_ts_ms` across video and cursor events
    - _Requirements: 5.1, 5.2, 5.5, 5.6, 5.7, 5.8, 5.10_

  - [x] 3.6 Write property test for event dispatch correctness
    - **Property 7: Event dispatch correctness**
    - Use `proptest` to generate arbitrary `RecordingEvent` variants
    - Assert `Video` events only affect `VideoProcessor` state, `Audio` only `AudioProcessor`, `Cursor` only `CursorProcessor`
    - **Validates: Requirement 5.1**

  - [x] 3.7 Write property test for stream termination completeness
    - **Property 9: Stream termination completeness**
    - Use `proptest` to generate all combinations of stream-ended events
    - Assert only the corresponding flag is set for each ended stream
    - Assert `all_streams_ended()` returns true iff all three flags are true
    - **Validates: Requirements 5.5, 5.6**

  - [x] 3.8 Write property test for timestamp monotonicity
    - **Property 10: Timestamp monotonicity**
    - Use `proptest` to generate sequences of video and cursor events with varying timestamps
    - Assert `last_observed_ts_ms` is monotonically non-decreasing after each event
    - **Validates: Requirement 5.8**

  - [x] 3.9 Write property test for audio error non-fatality
    - **Property 15: Audio error is non-fatal**
    - Use `proptest` to generate event sequences containing `AudioCaptureEvent::Error`
    - Assert coordinator sets `audio_ended = true` and continues processing video/cursor events
    - **Validates: Requirement 11.4**

  - [x] 3.10 Implement `RecordingCoordinator::finalize` producing `WorkerOutcome`
    - Flush primary and preview encoders, write mouse store, close PCM writers
    - Produce `WorkerOutcome` with final dimensions, pause intervals, and audio recording flags
    - _Requirements: 5.9_

- [x] 4. Checkpoint — Phase 2 complete
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Phase 3 — Adapter layer with per-adapter crossbeam channels
  - [x] 5.1 Define `StreamAdapter` trait and `AdapterCommand` enum
    - Create `snow-screen-recorder/src/adapter/mod.rs`
    - Define trait with `pause`, `resume`, `stop`, `is_running`, `join` methods
    - Trait must require `Send` for cross-thread usage
    - Define `AdapterCommand` enum with `Pause`, `Resume`, `Stop`
    - `stop` must be non-blocking; `join` must block until forwarding thread exits
    - `join` before `start` must return successfully without blocking
    - _Requirements: 2.1, 2.4, 2.5, 2.6, 2.10_

  - [x] 5.2 Implement `VideoStreamAdapter` with forwarding thread
    - Create `snow-screen-recorder/src/adapter/video.rs`
    - Implement `start` that spawns a forwarding thread owning `StreamHandle`
    - Return `AlreadyStarted` error if `start` called while already running
    - Forwarding thread bridges `std::sync::mpsc` → `crossbeam_channel` via `video_forward_loop`
    - Preserve backend timestamps from `CaptureEvent::Paused { at }` and `CaptureEvent::Resumed { at, gap }`
    - When `cursor` feature is enabled, extract `FrameMetadata::cursor` and forward as `RecordingEvent::Cursor` on cursor channel
    - Use `send_timeout` (default 10ms) under backpressure; poll command channel between retries
    - _Requirements: 2.2, 2.3, 2.7, 2.8, 2.9, 11.1, 11.5, 11.6_

  - [x] 5.3 Write property test for event translation fidelity
    - **Property 1: Event translation fidelity**
    - Use `proptest` to generate arbitrary leaf crate events (video, audio, cursor)
    - Translate through adapter and assert all payload fields are identical to originals
    - **Validates: Requirements 1.2, 1.3, 1.4, 2.6**

  - [x] 5.4 Write property test for embedded cursor extraction
    - **Property 2: Embedded cursor extraction**
    - Use `proptest` to generate video frames with non-None `FrameMetadata::cursor`
    - Assert both `RecordingEvent::Video` on video channel and `RecordingEvent::Cursor` on cursor channel are produced
    - Assert cursor event's `CursorFrameSample` matches frame's embedded cursor data
    - **Validates: Requirements 2.7, 9.5**

  - [x] 5.5 Implement `AudioStreamAdapter` with forwarding thread
    - Create `snow-screen-recorder/src/adapter/audio.rs`
    - Forwarding thread bridges `EventQueue` (Mutex + Condvar) → `crossbeam_channel`
    - Forward all audio lifecycle events (`Paused`, `Resumed`, `SourceRestarted`, `BufferPressure`)
    - Forward `AudioCaptureEvent::Error` when device disconnects and retries fail
    - Use `send_timeout` with configurable timeout; poll command channel between retries
    - _Requirements: 2.2, 2.3, 11.2, 11.5, 11.6_

  - [x] 5.6 Implement `CursorStreamAdapter` with polling thread
    - Create `snow-screen-recorder/src/adapter/cursor.rs`
    - Only created when `snow-capture` is built without the `cursor` feature
    - Poll `CursorSampler` at target FPS on a dedicated thread
    - Send `CursorCaptureEvent::Sample` events on cursor channel
    - _Requirements: 2.2, 9.7_

  - [x] 5.7 Create per-adapter channels and `RecordingAdapters` struct
    - Create separate crossbeam channels for video, audio, cursor, and control command streams
    - Channel capacities defined by configuration constants (video: 8, audio: smaller, cursor: 2-4)
    - Wire adapters into `RecordingAdapters` struct holding receivers and adapter objects
    - _Requirements: 3.1_

  - [x] 5.8 Implement `recording_worker` event loop with `crossbeam::select!`
    - Use `crossbeam::select!` across all per-adapter receivers and control receiver
    - Implement audio-first drain policy: drain up to `audio_drain_batch` (default 8) audio events before `select!` wait
    - Mark disconnected channels as closed, exclude from subsequent `select!` operations
    - Exit when all three data channels are disconnected
    - Use configurable select timeout (default 25ms) for all-channels-disconnected detection
    - Evaluate stop conditions in precedence order: `FatalVideoError`, `ControlStop`, `AllStreamsEnded`, `AllChannelsDisconnected`
    - Honor `ControlCommand::Stop` within 100ms under sustained backpressure
    - _Requirements: 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8, 11.3, 11.4, 11.7_

  - [x] 5.9 Write property test for audio-first drain priority
    - **Property 3: Audio-first drain priority**
    - Use `proptest` to generate event sequences with both audio and video events available
    - Assert up to 8 audio events are processed before entering `select!` wait
    - **Validates: Requirement 3.3**

  - [x] 5.10 Implement graceful shutdown and drain sequence
    - Signal all adapters to stop before joining forwarding threads
    - Continue draining per-adapter channels with audio-first priority while any adapter is still running
    - Join all adapter threads after forwarders exit
    - Perform final drain pass on all per-adapter channels
    - Process all drained events through RecordingCoordinator before finalization
    - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5_

  - [x] 5.11 Write property test for event completeness across shutdown
    - **Property 11: Event completeness across shutdown**
    - Use `proptest` to generate sets of events present in channels at stop time
    - Assert every event is processed by the coordinator before finalization
    - **Validates: Requirements 6.2, 6.5**

  - [x] 5.12 Implement error propagation and backpressure handling in forwarding threads
    - Forward `VideoCaptureEvent::Error` on unrecoverable capture backend errors
    - Forward `AudioCaptureEvent::Error` on audio device disconnect after retries fail
    - Use `send_timeout` with configurable timeout (default 10ms) under backpressure
    - Poll command channel after each `send_timeout` expiration before retrying
    - Configure overflow behavior per channel at startup; default policy `RetryUntilStop`, no silent drops
    - Emit per-subsystem diagnostics counters for timeout retries
    - _Requirements: 11.1, 11.2, 11.5, 11.6, 11.7, 11.8, 11.9_

- [x] 6. Checkpoint — Phase 3 complete
  - Ensure all tests pass, ask the user if questions arise.

- [x] 7. Phase 4 — Mouse type deduplication via From impls
  - [x] 7.1 Implement `From<CursorCompositionMode>` for `CursorShapeCompositionMode`
    - Add `From` impl in `snow-screen-recorder/src/mouse.rs`
    - Remove the manual `map_cursor_composition_mode()` function
    - _Requirements: 7.1, 7.4_

  - [x] 7.2 Implement `CursorShapeRecord::from_cursor_shape` and `CursorFrameRecord::from_sample`
    - Add `from_cursor_shape` converting from `snow_cursor_capture::CursorShape` with all fields matching
    - Add `from_sample` converting from `snow_cursor_capture::CursorFrameSample` with timestamp and coordinate translation
    - Remove all manual field-by-field conversion code scattered through `WorkerContext::record_cursor_frame()`
    - _Requirements: 7.2, 7.3, 7.4_

  - [x] 7.3 Write property test for cursor type conversion fidelity
    - **Property 12: Cursor type conversion fidelity**
    - Use `proptest` to generate arbitrary `CursorCompositionMode`, `CursorShape`, and `CursorFrameSample` values
    - Assert `From` conversion preserves semantic equivalence
    - Assert `from_cursor_shape` produces records with all fields matching source
    - Assert `from_sample` produces records with translated coordinates and matching fields
    - **Validates: Requirements 7.1, 7.2, 7.3**

- [x] 8. Phase 5 — Decoupled RecordingConfig types
  - [x] 8.1 Define `MonitorSelector`, `WindowSelector`, and `RecordingRegion` types
    - Create own types in `snow-screen-recorder` that don't expose `snow_capture` internals
    - `MonitorSelector` wraps `stable_id` string in `"{adapter_luid:016x}-{output_id:016x}"` format
    - `WindowSelector` wraps `raw_handle: isize`
    - `RecordingRegion` defines `x: i32`, `y: i32`, `width: u32`, `height: u32`
    - _Requirements: 8.1, 8.2, 8.3, 8.4_

  - [x] 8.2 Update `RecordingTarget` enum to use new selector types
    - Replace `snow_capture::MonitorId`, `WindowId`, `CaptureRegion` with `MonitorSelector`, `WindowSelector`, `RecordingRegion`
    - Deprecate sub-crate type re-exports
    - _Requirements: 8.1_

  - [x] 8.3 Implement `resolve_capture_target` in `VideoStreamAdapter`
    - Resolve `MonitorSelector` to `snow_capture::MonitorId` by enumerating monitors and matching by `stable_id`
    - Return `InvalidConfig` error with unresolved `stable_id` if monitor is disconnected
    - Validate `WindowSelector` resolves to a live capturable window
    - Return `InvalidConfig` error with unresolved `raw_handle` if window validation fails
    - Forward `VideoCaptureEvent::Error` if selected window becomes unavailable during recording
    - _Requirements: 8.5, 8.6, 8.7, 8.8, 8.9_

  - [x] 8.4 Write property test for MonitorSelector resolution
    - **Property 13: MonitorSelector resolution**
    - Use `proptest` to generate sets of available monitors and `MonitorSelector` values
    - Assert matching `stable_id` returns correct `MonitorId`
    - Assert non-matching `stable_id` returns `InvalidConfig` error containing the unresolved `stable_id`
    - **Validates: Requirements 8.2, 8.5**

- [x] 9. Checkpoint — Phases 4-5 complete
  - Ensure all tests pass, ask the user if questions arise.

- [x] 10. Phase 6 — Cursor capture decoupling via feature flag
  - [x] 10.1 Make `snow-cursor-capture` optional in `snow-capture` via `cursor` feature flag
    - Update `snow-capture/Cargo.toml`: add `cursor` feature defaulting to enabled, make `snow-cursor-capture` optional dep
    - Gate `FrameMetadata::cursor` field with `#[cfg(feature = "cursor")]`
    - Ensure `cursor: Option<CursorData>` is stable in both feature states (always `None` when disabled)
    - Keep `CursorData` type available without requiring `snow-cursor-capture` linkage when disabled
    - **This is a semver-breaking change**: bump `snow-capture` to `0.2.0`
    - _Requirements: 9.1, 9.2, 9.3, 9.4_

  - [x] 10.2 Wire `cursor` feature forwarding in `snow-screen-recorder`
    - Add `cursor = ["snow-capture/cursor"]` feature in `snow-screen-recorder/Cargo.toml`
    - When `cursor` enabled: `VideoStreamAdapter` extracts cursor from frames, no `CursorStreamAdapter` created
    - When `cursor` disabled: `RecordingSession` creates standalone `CursorStreamAdapter` polling at target FPS
    - Use `#[cfg(feature = "cursor")]` / `#[cfg(not(feature = "cursor"))]` for compile-time path selection
    - _Requirements: 9.5, 9.6, 9.7, 9.8_

  - [x] 10.3 Write property test for cursor data path exclusivity
    - **Property 14: Cursor data path exclusivity**
    - Verify at compile time that cursor data arrives through exactly one path
    - Test both `cursor`-enabled and `cursor`-disabled configurations
    - Assert no duplicate cursor records from both paths simultaneously
    - **Validates: Requirements 9.5, 9.7**

  - [x] 10.4 Verify workspace compiles in both cursor feature configurations
    - Run `cargo check` with default features (cursor enabled)
    - Run `cargo check --no-default-features` (cursor disabled)
    - Run targeted recorder tests in both configurations
    - _Requirements: 9.9_

- [x] 11. Testability and mock adapter framework
  - [x] 11.1 Implement mock adapter framework for testing
    - Create mock `StreamAdapter` implementations that write directly to crossbeam channels
    - Support scripted sequences: ordered events, channel disconnects, injected errors
    - Ensure `RecordingCoordinator` processes mock events identically to real adapter events
    - Support injectable clock/timestamp source for deterministic time progression
    - _Requirements: 10.1, 10.2, 10.4, 10.5_

  - [x] 11.2 Write deterministic termination condition tests
    - Create test scenarios covering each `TerminationCondition` path
    - Test `FatalVideoError`: video error stops loop, remaining audio/cursor drained
    - Test `ControlStop`: control command honored within 100ms
    - Test `AllStreamsEnded`: all three stream-ended flags trigger stop
    - Test `AllChannelsDisconnected`: all channels disconnected triggers exit
    - _Requirements: 10.6, 11.3, 11.4_

  - [x] 11.3 Write processor unit tests with synthetic data
    - Test `VideoProcessor` with synthetic RGBA buffers (encoder creation, duplicates, resolution changes, preview encoder)
    - Test `AudioProcessor` with synthetic PCM data (routing, silence insertion, format conversion)
    - Test `CursorProcessor` with synthetic samples (dedup, translation, frame recording)
    - _Requirements: 10.3_

- [x] 12. Final checkpoint — All phases complete
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation between phases
- Property tests validate universal correctness properties from the design document
- The 6-phase migration path allows incremental refactoring with no API changes until Phase 5-6
- Phase 6 (cursor feature flag) is a semver-breaking change requiring `snow-capture` 0.2.0 bump
- `snow-capture` uses `std::sync::mpsc::sync_channel`, NOT crossbeam — forwarding threads bridge this gap
- `snow-audio-recorder` uses a custom `EventQueue` (Mutex + Condvar), NOT mpsc — forwarding threads bridge this gap
- `VideoProcessor` must preserve the `preview_encoder` field from the current `WorkerContext`
