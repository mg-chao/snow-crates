use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;

use snow_core::error::RecvTimeoutError;
use snow_core::streaming::StreamHandle;

use crate::adapter::{AdapterCommand, StreamAdapter};
use crate::error::{Result, ScreenRecorderError};
use crate::event::RecordingEvent;

/// Default recv timeout for polling the source handle (25ms).
const RECV_TIMEOUT: Duration = Duration::from_millis(25);

/// Generic adapter that bridges any `StreamHandle<E>` into a crossbeam channel.
///
/// `E` - event type from the leaf crate.
/// `F` - mapping closure `Fn(E) -> RecordingEvent`.
///
/// Replaces the near-identical `VideoStreamAdapter` and `AudioStreamAdapter`
/// forwarding-thread implementations with a single generic version.
pub(crate) struct StreamBridge<E, F>
where
    E: Send + 'static,
    F: Fn(E) -> RecordingEvent + Send + 'static,
{
    cmd_tx: crossbeam_channel::Sender<AdapterCommand>,
    running: Arc<AtomicBool>,
    forward_thread: Option<std::thread::JoinHandle<Result<()>>>,
    _phantom: PhantomData<(E, F)>,
}

impl<E, F> StreamBridge<E, F>
where
    E: Send + 'static,
    F: Fn(E) -> RecordingEvent + Send + 'static,
{
    /// Start the forwarding thread.
    ///
    /// - `handle`: Any leaf crate handle implementing `StreamHandle<E>`.
    /// - `mapper`: Converts leaf events to `RecordingEvent`.
    /// - `event_tx`: Crossbeam sender for the coordinator.
    /// - `send_timeout`: Backpressure retry timeout.
    /// - `thread_name`: Name for the spawned forwarding thread.
    pub fn start<H>(
        handle: H,
        mapper: F,
        event_tx: Sender<RecordingEvent>,
        send_timeout: Duration,
        thread_name: &str,
    ) -> Result<Self>
    where
        H: StreamHandle<E> + 'static,
    {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AdapterCommand>();
        let running = Arc::new(AtomicBool::new(true));
        let running_flag = running.clone();

        let forward_thread = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let result = forward_loop(handle, mapper, event_tx, cmd_rx, send_timeout);
                running_flag.store(false, Ordering::Release);
                result
            })
            .map_err(ScreenRecorderError::Io)?;

        Ok(Self {
            cmd_tx,
            running,
            forward_thread: Some(forward_thread),
            _phantom: PhantomData,
        })
    }
}

impl<E, F> StreamAdapter for StreamBridge<E, F>
where
    E: Send + 'static,
    F: Fn(E) -> RecordingEvent + Send + 'static,
{
    fn pause(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Pause)
            .map_err(|_| ScreenRecorderError::Encode("stream bridge command channel closed".into()))
    }

    fn resume(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Resume)
            .map_err(|_| ScreenRecorderError::Encode("stream bridge command channel closed".into()))
    }

    fn stop(&self) -> Result<()> {
        self.cmd_tx
            .send(AdapterCommand::Stop)
            .map_err(|_| ScreenRecorderError::Encode("stream bridge command channel closed".into()))
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    fn join(&mut self) -> Result<()> {
        if let Some(handle) = self.forward_thread.take() {
            handle
                .join()
                .map_err(|_| ScreenRecorderError::Encode("stream bridge thread panicked".into()))?
        } else {
            Ok(())
        }
    }
}

/// The generic forwarding loop that bridges any `StreamHandle<E>` -> crossbeam channel.
///
/// Owns the handle and polls it for events via `recv_timeout`. Maps each event
/// through `mapper` and sends the result on `event_tx`. Polls `cmd_rx` on
/// timeouts and during backpressure to honor pause/resume/stop commands.
///
/// Stopped detection: when `recv_timeout` returns `Disconnected`, the loop
/// checks `handle.is_running()`:
/// - `true` -> unexpected disconnect, source still alive but channel closed
/// - `false` -> clean stop (someone called `stop()`), exit without sentinel
fn forward_loop<E, H, F>(
    handle: H,
    mapper: F,
    event_tx: Sender<RecordingEvent>,
    cmd_rx: crossbeam_channel::Receiver<AdapterCommand>,
    send_timeout: Duration,
) -> Result<()>
where
    E: Send + 'static,
    H: StreamHandle<E> + 'static,
    F: Fn(E) -> RecordingEvent + Send + 'static,
{
    loop {
        // Always drain control commands before polling the source handle.
        // Without this, hot streams that never hit timeout/backpressure can
        // starve Pause/Resume/Stop indefinitely.
        match drain_commands(&cmd_rx, &handle) {
            CommandResult::Continue => {}
            CommandResult::Stop => break,
        }

        match handle.recv_timeout(RECV_TIMEOUT).map_err(Into::into) {
            Ok(event) => {
                let recording_event = mapper(event);
                if send_with_backpressure(
                    &event_tx,
                    recording_event,
                    &cmd_rx,
                    &handle,
                    send_timeout,
                )
                .is_break()
                {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => match drain_commands(&cmd_rx, &handle) {
                CommandResult::Continue => {}
                CommandResult::Stop => break,
            },
            Err(RecvTimeoutError::Disconnected) => {
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
/// to the stream handle. Returns `Stop` if a stop command was received.
fn drain_commands<E, H>(
    cmd_rx: &crossbeam_channel::Receiver<AdapterCommand>,
    handle: &H,
) -> CommandResult
where
    H: StreamHandle<E>,
{
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            AdapterCommand::Pause => handle.pause(),
            AdapterCommand::Resume => handle.resume(),
            AdapterCommand::Stop => {
                handle.stop();
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
/// Uses `send_timeout` with the configured duration and polls the command
/// channel between retries. Returns `Break` if the channel disconnected
/// or a stop command was received during backpressure.
fn send_with_backpressure<E, H>(
    tx: &Sender<RecordingEvent>,
    event: RecordingEvent,
    cmd_rx: &crossbeam_channel::Receiver<AdapterCommand>,
    handle: &H,
    send_timeout: Duration,
) -> SendOutcome
where
    H: StreamHandle<E>,
{
    let mut event = event;
    loop {
        match tx.send_timeout(event, send_timeout) {
            Ok(()) => return SendOutcome::Sent,
            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => {
                event = returned;
                match drain_commands(cmd_rx, handle) {
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use snow_core::error::{
        RecvError as CoreRecvError, RecvTimeoutError as CoreRecvTimeoutError,
        TryRecvError as CoreTryRecvError,
    };
    use snow_core::streaming::StreamHandle;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::adapter::StreamAdapter;
    use crate::event::{RecordingEvent, VideoCaptureEvent};

    use super::StreamBridge;

    struct MockStreamHandle<E: Clone + Send> {
        events: Vec<E>,
        index: Arc<AtomicUsize>,
        stopped: Arc<AtomicBool>,
    }

    impl<E: Clone + Send> MockStreamHandle<E> {
        fn new(events: Vec<E>) -> Self {
            Self {
                events,
                index: Arc::new(AtomicUsize::new(0)),
                stopped: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl<E: Clone + Send> StreamHandle<E> for MockStreamHandle<E> {
        type RecvError = CoreRecvError;
        type TryRecvError = CoreTryRecvError;
        type RecvTimeoutError = CoreRecvTimeoutError;

        fn recv(&self) -> std::result::Result<E, Self::RecvError> {
            let i = self.index.fetch_add(1, Ordering::SeqCst);
            if i < self.events.len() {
                Ok(self.events[i].clone())
            } else {
                self.stopped.store(true, Ordering::Release);
                Err(CoreRecvError::Disconnected)
            }
        }

        fn try_recv(&self) -> std::result::Result<E, Self::TryRecvError> {
            let i = self.index.fetch_add(1, Ordering::SeqCst);
            if i < self.events.len() {
                Ok(self.events[i].clone())
            } else {
                self.stopped.store(true, Ordering::Release);
                Err(CoreTryRecvError::Disconnected)
            }
        }

        fn recv_timeout(
            &self,
            _timeout: Duration,
        ) -> std::result::Result<E, Self::RecvTimeoutError> {
            let i = self.index.fetch_add(1, Ordering::SeqCst);
            if i < self.events.len() {
                Ok(self.events[i].clone())
            } else {
                self.stopped.store(true, Ordering::Release);
                Err(CoreRecvTimeoutError::Disconnected)
            }
        }

        fn stop(&self) {
            self.stopped.store(true, Ordering::Release);
        }

        fn pause(&self) {}
        fn resume(&self) {}

        fn is_paused(&self) -> bool {
            false
        }

        fn is_running(&self) -> bool {
            !self.stopped.load(Ordering::Acquire)
        }
    }

    #[derive(Default)]
    struct CommandCounters {
        pause_calls: AtomicUsize,
        resume_calls: AtomicUsize,
        stop_calls: AtomicUsize,
    }

    struct CommandResponsiveStreamHandle {
        counters: Arc<CommandCounters>,
        produced: AtomicUsize,
        produced_limit: usize,
        stopped: AtomicBool,
        per_event_delay: Duration,
    }

    impl CommandResponsiveStreamHandle {
        fn new(
            counters: Arc<CommandCounters>,
            produced_limit: usize,
            per_event_delay: Duration,
        ) -> Self {
            Self {
                counters,
                produced: AtomicUsize::new(0),
                produced_limit,
                stopped: AtomicBool::new(false),
                per_event_delay,
            }
        }
    }

    impl StreamHandle<u8> for CommandResponsiveStreamHandle {
        type RecvError = CoreRecvError;
        type TryRecvError = CoreTryRecvError;
        type RecvTimeoutError = CoreRecvTimeoutError;

        fn recv(&self) -> std::result::Result<u8, Self::RecvError> {
            self.recv_timeout(Duration::from_millis(0))
                .map_err(|_| CoreRecvError::Disconnected)
        }

        fn try_recv(&self) -> std::result::Result<u8, Self::TryRecvError> {
            self.recv_timeout(Duration::from_millis(0))
                .map_err(|_| CoreTryRecvError::Disconnected)
        }

        fn recv_timeout(
            &self,
            _timeout: Duration,
        ) -> std::result::Result<u8, Self::RecvTimeoutError> {
            if self.stopped.load(Ordering::Acquire) {
                return Err(CoreRecvTimeoutError::Disconnected);
            }

            let produced = self.produced.fetch_add(1, Ordering::AcqRel);
            if produced >= self.produced_limit {
                self.stopped.store(true, Ordering::Release);
                return Err(CoreRecvTimeoutError::Disconnected);
            }

            std::thread::sleep(self.per_event_delay);
            Ok(1)
        }

        fn stop(&self) {
            self.counters.stop_calls.fetch_add(1, Ordering::AcqRel);
            self.stopped.store(true, Ordering::Release);
        }

        fn pause(&self) {
            self.counters.pause_calls.fetch_add(1, Ordering::AcqRel);
        }

        fn resume(&self) {
            self.counters.resume_calls.fetch_add(1, Ordering::AcqRel);
        }

        fn is_paused(&self) -> bool {
            false
        }

        fn is_running(&self) -> bool {
            !self.stopped.load(Ordering::Acquire)
        }
    }

    #[test]
    fn control_commands_are_drained_on_hot_streams() {
        let (event_tx, _event_rx) = crossbeam_channel::bounded::<RecordingEvent>(512);
        let counters = Arc::new(CommandCounters::default());
        let handle = CommandResponsiveStreamHandle::new(
            Arc::clone(&counters),
            200,
            Duration::from_millis(1),
        );

        let mut bridge = StreamBridge::start(
            handle,
            |_v: u8| RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence: 1 }),
            event_tx,
            Duration::from_millis(10),
            "test-hot-stream-bridge",
        )
        .expect("bridge should start");

        std::thread::sleep(Duration::from_millis(10));
        bridge.pause().expect("pause command should enqueue");
        bridge.resume().expect("resume command should enqueue");
        bridge.stop().expect("stop command should enqueue");
        bridge
            .join()
            .expect("bridge should stop after control commands");

        assert_eq!(
            counters.pause_calls.load(Ordering::Acquire),
            1,
            "pause should be propagated to handle exactly once"
        );
        assert_eq!(
            counters.resume_calls.load(Ordering::Acquire),
            1,
            "resume should be propagated to handle exactly once"
        );
        assert_eq!(
            counters.stop_calls.load(Ordering::Acquire),
            1,
            "stop should be propagated to handle exactly once"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]
        #[test]
        fn prop_stream_bridge_forwards_all_events_in_order(
            values in prop::collection::vec(0u32..u32::MAX, 1..50)
        ) {
            let (event_tx, event_rx) = crossbeam_channel::unbounded::<RecordingEvent>();

            let expected: Vec<u64> = values.iter().map(|&v| v as u64).collect();

            let handle = MockStreamHandle::new(values);

            let mut bridge = StreamBridge::start(
                handle,
                |v: u32| RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence: v as u64 }),
                event_tx,
                Duration::from_millis(100),
                "test-bridge",
            )
            .expect("bridge should start");

            bridge.join().expect("bridge join should succeed");

            let mut received = Vec::new();
            while let Ok(evt) = event_rx.try_recv() {
                match evt {
                    RecordingEvent::Video(VideoCaptureEvent::FrameDropped { sequence }) => {
                        received.push(sequence);
                    }
                    _ => panic!("unexpected event variant"),
                }
            }

            prop_assert_eq!(received, expected);
        }
    }
}
