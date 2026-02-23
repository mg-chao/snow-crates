pub(crate) mod com;
pub(crate) mod convert;
pub(crate) mod device_enum;
pub(crate) mod hresult;
pub(crate) mod notification;
pub(crate) mod wasapi_source;

use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::backend::{AudioBackend, AudioBackendKind, AudioRecorderEngine, EngineEvent};
use crate::device::{AudioDeviceInfo, DeviceFlow};
use crate::error::{AudioError, AudioResult};
use crate::packet::{AudioEvent, AudioSourceKind};
use crate::session::{AudioStreamConfig, SourceConfig};

use self::com::{CoInitGuard, EventHandle};
use self::notification::NotificationClientGuard;
use self::wasapi_source::WasapiSource;

pub(crate) struct WasapiBackend {
    kind: AudioBackendKind,
}

impl WasapiBackend {
    pub fn new(kind: AudioBackendKind) -> AudioResult<Self> {
        match kind {
            AudioBackendKind::Auto | AudioBackendKind::Wasapi => Ok(Self { kind }),
        }
    }
}

impl AudioBackend for WasapiBackend {
    fn enumerate_devices(&self, flow: DeviceFlow) -> AudioResult<Vec<AudioDeviceInfo>> {
        let _coinit = CoInitGuard::init_multithreaded()?;
        let enumerator = device_enum::create_device_enumerator()?;
        device_enum::enumerate_devices(&enumerator, flow)
    }

    fn create_engine(&self, config: AudioStreamConfig) -> AudioResult<Box<dyn AudioRecorderEngine>> {
        match self.kind {
            AudioBackendKind::Auto | AudioBackendKind::Wasapi => {
                Ok(Box::new(WasapiEngine::new(config)))
            }
        }
    }
}

struct WasapiEngine {
    config: AudioStreamConfig,
    initialized: bool,
    _coinit: Option<CoInitGuard>,
    enumerator: Option<windows::Win32::Media::Audio::IMMDeviceEnumerator>,
    control_event: Option<Arc<EventHandle>>,
    notification: Option<NotificationClientGuard>,
    system_source: Option<WasapiSource>,
    microphone_source: Option<WasapiSource>,
}

// SAFETY: WasapiEngine is moved into and used by a single dedicated worker thread.
// All COM interfaces are initialized in MTA on that thread and never shared.
unsafe impl Send for WasapiEngine {}

impl WasapiEngine {
    fn new(config: AudioStreamConfig) -> Self {
        Self {
            config,
            initialized: false,
            _coinit: None,
            enumerator: None,
            control_event: None,
            notification: None,
            system_source: None,
            microphone_source: None,
        }
    }

    fn ensure_initialized(&mut self) -> AudioResult<()> {
        if self.initialized {
            return Ok(());
        }

        let coinit = CoInitGuard::init_multithreaded()?;
        let enumerator = device_enum::create_device_enumerator()?;
        let control_event = Arc::new(EventHandle::new_manual_reset(false)?);
        let notification = NotificationClientGuard::register(&enumerator, Arc::clone(&control_event))?;

        self._coinit = Some(coinit);
        self.enumerator = Some(enumerator.clone());
        self.control_event = Some(control_event);
        self.notification = Some(notification);

        if self.config.system.enabled {
            match WasapiSource::new(AudioSourceKind::System, self.config.system.clone(), enumerator.clone()) {
                Ok(source) => self.system_source = Some(source),
                Err(err) if self.config.system.required => return Err(err),
                Err(_) => self.system_source = None,
            }
        }

        if self.config.microphone.enabled {
            match WasapiSource::new(
                AudioSourceKind::Microphone,
                self.config.microphone.clone(),
                enumerator,
            ) {
                Ok(source) => self.microphone_source = Some(source),
                Err(err) if self.config.microphone.required => return Err(err),
                Err(_) => self.microphone_source = None,
            }
        }

        if self.system_source.is_none() && self.microphone_source.is_none() {
            return Err(AudioError::DeviceUnavailable(
                "no enabled audio source could be initialized".into(),
            ));
        }

        self.initialized = true;
        Ok(())
    }

    fn control_handle(&self) -> AudioResult<windows::Win32::Foundation::HANDLE> {
        self.control_event
            .as_ref()
            .map(|ev| ev.raw())
            .ok_or_else(|| AudioError::WorkerDead)
    }

    fn process_control_notifications(&mut self) -> AudioResult<Vec<AudioEvent>> {
        let mut events = Vec::new();

        if !self.config.restart_policy.auto_rebind_on_default_change {
            if let Some(notification) = &self.notification {
                let state = notification.state();
                let _ = state.take_render_default_changed();
                let _ = state.take_capture_default_changed();
                let _ = state.take_topology_changed();
            }
            return Ok(events);
        }

        let (render_changed, capture_changed, topology_changed) = if let Some(notification) = &self.notification {
            let state = notification.state();
            (
                state.take_render_default_changed(),
                state.take_capture_default_changed(),
                state.take_topology_changed(),
            )
        } else {
            (false, false, false)
        };

        if render_changed && self.config.system.enabled {
            if matches!(self.config.system.device, crate::device::DeviceSelector::DefaultRender) {
                if let Some(event) = self.restart_system_source()? {
                    events.push(event);
                }
            }
        }

        if capture_changed && self.config.microphone.enabled {
            if matches!(
                self.config.microphone.device,
                crate::device::DeviceSelector::DefaultCapture
            ) {
                if let Some(event) = self.restart_microphone_source()? {
                    events.push(event);
                }
            }
        }

        if topology_changed {
            if self.system_source.is_none() && self.config.system.enabled && !self.config.system.required {
                if let Some(event) = self.restart_system_source()? {
                    events.push(event);
                }
            }
            if self.microphone_source.is_none()
                && self.config.microphone.enabled
                && !self.config.microphone.required
            {
                if let Some(event) = self.restart_microphone_source()? {
                    events.push(event);
                }
            }
        }

        Ok(events)
    }

    fn restart_system_source(&mut self) -> AudioResult<Option<AudioEvent>> {
        restart_source_with_policy(
            &mut self.system_source,
            &self.config.system,
            AudioSourceKind::System,
            self.enumerator
                .as_ref()
                .cloned()
                .ok_or(AudioError::WorkerDead)?,
            self.config.restart_policy.max_attempts,
            self.config.restart_policy.initial_backoff,
            self.config.restart_policy.max_backoff,
        )
    }

    fn restart_microphone_source(&mut self) -> AudioResult<Option<AudioEvent>> {
        restart_source_with_policy(
            &mut self.microphone_source,
            &self.config.microphone,
            AudioSourceKind::Microphone,
            self.enumerator
                .as_ref()
                .cloned()
                .ok_or(AudioError::WorkerDead)?,
            self.config.restart_policy.max_attempts,
            self.config.restart_policy.initial_backoff,
            self.config.restart_policy.max_backoff,
        )
    }

    fn drain_system(&mut self, out: &mut Vec<AudioEvent>) -> AudioResult<()> {
        if self.system_source.is_none() {
            return Ok(());
        }

        let result = self
            .system_source
            .as_mut()
            .unwrap()
            .drain_packets();

        match result {
            Ok(packets) => {
                out.extend(packets.into_iter().map(AudioEvent::Packet));
                Ok(())
            }
            Err(err) if err.is_retryable() || err.requires_worker_reset() => {
                if let Some(event) = self.restart_system_source()? {
                    out.push(event);
                    Ok(())
                } else if self.config.system.required {
                    Err(err)
                } else {
                    Ok(())
                }
            }
            Err(err) => Err(err),
        }
    }

    fn drain_microphone(&mut self, out: &mut Vec<AudioEvent>) -> AudioResult<()> {
        if self.microphone_source.is_none() {
            return Ok(());
        }

        let result = self
            .microphone_source
            .as_mut()
            .unwrap()
            .drain_packets();

        match result {
            Ok(packets) => {
                out.extend(packets.into_iter().map(AudioEvent::Packet));
                Ok(())
            }
            Err(err) if err.is_retryable() || err.requires_worker_reset() => {
                if let Some(event) = self.restart_microphone_source()? {
                    out.push(event);
                    Ok(())
                } else if self.config.microphone.required {
                    Err(err)
                } else {
                    Ok(())
                }
            }
            Err(err) => Err(err),
        }
    }
}

impl AudioRecorderEngine for WasapiEngine {
    fn poll(&mut self, timeout: Duration) -> AudioResult<EngineEvent> {
        self.ensure_initialized()?;

        let mut handles = vec![self.control_handle()?];
        if let Some(source) = &self.system_source {
            handles.push(source.event_handle());
        }
        if let Some(source) = &self.microphone_source {
            handles.push(source.event_handle());
        }

        if handles.is_empty() {
            return Ok(EngineEvent::Idle);
        }

        let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        let wait_result = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };

        if wait_result == WAIT_TIMEOUT {
            return Ok(EngineEvent::Idle);
        }

        if wait_result == WAIT_FAILED {
            return Err(AudioError::platform(anyhow::anyhow!(
                "WaitForMultipleObjects failed"
            )));
        }

        let mut events = Vec::new();

        if wait_result == WAIT_OBJECT_0 {
            if let Some(control) = &self.control_event {
                let _ = control.reset();
            }
            events.extend(self.process_control_notifications()?);
        }

        self.drain_system(&mut events)?;
        self.drain_microphone(&mut events)?;

        if events.is_empty() {
            Ok(EngineEvent::Idle)
        } else {
            Ok(EngineEvent::Events(events))
        }
    }
}

fn restart_source_with_policy(
    slot: &mut Option<WasapiSource>,
    config: &SourceConfig,
    source_kind: AudioSourceKind,
    enumerator: windows::Win32::Media::Audio::IMMDeviceEnumerator,
    max_attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
) -> AudioResult<Option<AudioEvent>> {
    if !config.enabled {
        *slot = None;
        return Ok(None);
    }

    let start = Instant::now();
    let mut backoff = initial_backoff;
    let attempts = max_attempts.max(1);

    for attempt_idx in 0..attempts {
        let result = if let Some(source) = slot.as_mut() {
            source.restart()
        } else {
            match WasapiSource::new(source_kind, config.clone(), enumerator.clone()) {
                Ok(source) => {
                    let new_id = source.current_device_id().to_string();
                    *slot = Some(source);
                    Ok((None, new_id))
                }
                Err(err) => Err(err),
            }
        };

        match result {
            Ok((old_device_id, new_device_id)) => {
                return Ok(Some(AudioEvent::SourceRestarted {
                    source: source_kind,
                    old_device_id,
                    new_device_id,
                    downtime: start.elapsed(),
                }));
            }
            Err(err) => {
                if attempt_idx + 1 >= attempts {
                    if config.required {
                        return Err(err);
                    }
                    *slot = None;
                    return Ok(None);
                }

                std::thread::sleep(backoff);
                backoff = next_backoff(backoff, max_backoff);
            }
        }
    }

    Ok(None)
}

fn next_backoff(current: Duration, max_backoff: Duration) -> Duration {
    current.saturating_mul(2).min(max_backoff)
}
