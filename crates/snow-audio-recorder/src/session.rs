use std::sync::Arc;
use std::time::Duration;

use crate::backend::{self, AudioBackend, AudioBackendKind};
use crate::device::{AudioDeviceInfo, DeviceFlow, DeviceSelector};
use crate::error::AudioResult;
use crate::format::{AudioFormat, AudioSampleFormat};
use crate::streaming::AudioStreamHandle;

#[derive(Clone, Debug)]
pub struct SourceConfig {
    pub enabled: bool,
    pub required: bool,
    pub device: DeviceSelector,
    pub output_format: AudioFormat,
    pub packet_duration: Duration,
}

impl SourceConfig {
    pub fn default_system() -> Self {
        Self {
            enabled: true,
            required: true,
            device: DeviceSelector::DefaultRender,
            output_format: AudioFormat::new(48_000, 2, AudioSampleFormat::F32),
            packet_duration: Duration::from_millis(10),
        }
    }

    pub fn default_microphone() -> Self {
        Self {
            enabled: true,
            required: true,
            device: DeviceSelector::DefaultCapture,
            output_format: AudioFormat::new(48_000, 1, AudioSampleFormat::F32),
            packet_duration: Duration::from_millis(10),
        }
    }

    pub fn validate(&self) -> AudioResult<()> {
        if self.enabled {
            self.output_format.validate()?;
            if self.packet_duration.is_zero() {
                return Err(crate::error::AudioError::InvalidConfig(
                    "packet duration must be greater than zero".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RestartPolicy {
    pub auto_rebind_on_default_change: bool,
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            auto_rebind_on_default_change: true,
            max_attempts: 8,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AudioStreamConfig {
    pub system: SourceConfig,
    pub microphone: SourceConfig,
    pub event_buffer_depth: usize,
    pub max_consecutive_errors: usize,
    pub restart_policy: RestartPolicy,
    /// Fill ratio (`0.0..=1.0`) at which a `BufferPressure` event is emitted.
    /// Set to `1.0` to disable proactive pressure notifications.
    /// Defaults to `0.75`.
    pub backpressure_threshold: f64,
}

impl Default for AudioStreamConfig {
    fn default() -> Self {
        Self {
            system: SourceConfig::default_system(),
            microphone: SourceConfig::default_microphone(),
            event_buffer_depth: 128,
            max_consecutive_errors: 30,
            restart_policy: RestartPolicy::default(),
            backpressure_threshold: 0.75,
        }
    }
}

impl AudioStreamConfig {
    pub fn validate(&self) -> AudioResult<()> {
        self.system.validate()?;
        self.microphone.validate()?;

        if !self.system.enabled && !self.microphone.enabled {
            return Err(crate::error::AudioError::InvalidConfig(
                "at least one audio source must be enabled".into(),
            ));
        }

        if self.event_buffer_depth == 0 {
            return Err(crate::error::AudioError::InvalidConfig(
                "event buffer depth must be greater than zero".into(),
            ));
        }

        if self.max_consecutive_errors == 0 {
            return Err(crate::error::AudioError::InvalidConfig(
                "max_consecutive_errors must be greater than zero".into(),
            ));
        }

        if self.restart_policy.max_attempts == 0 {
            return Err(crate::error::AudioError::InvalidConfig(
                "restart policy max_attempts must be greater than zero".into(),
            ));
        }

        if self.restart_policy.initial_backoff.is_zero() || self.restart_policy.max_backoff.is_zero()
        {
            return Err(crate::error::AudioError::InvalidConfig(
                "restart backoff durations must be greater than zero".into(),
            ));
        }

        if self.restart_policy.initial_backoff > self.restart_policy.max_backoff {
            return Err(crate::error::AudioError::InvalidConfig(
                "restart initial_backoff must be <= max_backoff".into(),
            ));
        }

        if !(0.0..=1.0).contains(&self.backpressure_threshold) {
            return Err(crate::error::AudioError::InvalidConfig(
                "backpressure_threshold must be between 0.0 and 1.0".into(),
            ));
        }

        Ok(())
    }
}

pub struct AudioSessionBuilder {
    backend_override: Option<Arc<dyn AudioBackend>>,
    backend_kind: AudioBackendKind,
}

impl AudioSessionBuilder {
    pub fn new() -> Self {
        Self {
            backend_override: None,
            backend_kind: AudioBackendKind::Auto,
        }
    }

    pub fn with_backend(mut self, backend: Arc<dyn AudioBackend>) -> Self {
        self.backend_override = Some(backend);
        self
    }

    pub fn with_backend_kind(mut self, kind: AudioBackendKind) -> Self {
        self.backend_kind = kind;
        self.backend_override = None;
        self
    }

    pub fn build(self) -> AudioResult<AudioSession> {
        let backend = match self.backend_override {
            Some(backend) => backend,
            None => backend::backend_for_kind(self.backend_kind)?,
        };

        Ok(AudioSession { backend })
    }
}

impl Default for AudioSessionBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AudioSession {
    backend: Arc<dyn AudioBackend>,
}

impl AudioSession {
    pub fn builder() -> AudioSessionBuilder {
        AudioSessionBuilder::new()
    }

    pub fn new() -> AudioResult<Self> {
        Self::builder().build()
    }

    pub fn enumerate_render_devices(&self) -> AudioResult<Vec<AudioDeviceInfo>> {
        self.backend.enumerate_devices(DeviceFlow::Render)
    }

    pub fn enumerate_capture_devices(&self) -> AudioResult<Vec<AudioDeviceInfo>> {
        self.backend.enumerate_devices(DeviceFlow::Capture)
    }

    pub fn start_streaming(&self, config: AudioStreamConfig) -> AudioResult<AudioStreamHandle> {
        config.validate()?;
        let engine = self.backend.create_engine(config.clone())?;
        AudioStreamHandle::start(engine, config)
    }
}
