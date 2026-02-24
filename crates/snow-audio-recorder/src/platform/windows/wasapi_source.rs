use std::collections::VecDeque;
use std::mem::{size_of, size_of_val};
use std::ptr;
use std::slice;
use std::time::Instant;

use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_UNSUPPORTED_FORMAT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_NOPERSIST,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::System::Com::{CLSCTX_ALL, CoTaskMemFree};

use crate::device::{DeviceFlow, DeviceSelector};
use crate::error::{AudioError, AudioResult};
use crate::format::{AudioFormat, AudioSampleFormat};
use crate::packet::{
    AudioPacket, AudioPacketMetadata, AudioSourceKind, frames_to_100ns, frames_to_duration,
};
use crate::session::SourceConfig;

use super::com::EventHandle;
use super::convert::{AudioConverter, NativeAudioFormat, NativeSampleFormat};
use super::device_enum;
use super::hresult::map_hresult;

const IEEE_FLOAT_FORMAT_TAG: u16 = 3;
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

struct SourceRuntime {
    device_id: String,
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    native_format: NativeAudioFormat,
}

#[derive(Clone, Debug, Default)]
struct PendingMetadata {
    capture_time: Option<Instant>,
    qpc_position_100ns: Option<i64>,
    device_position_frames: Option<u64>,
    discontinuity: bool,
    is_silent: bool,
}

struct BufferedChunk {
    data: Vec<u8>,
    frames: u32,
    consumed_frames: u32,
    metadata: PendingMetadata,
}

impl BufferedChunk {
    fn remaining_frames(&self) -> u32 {
        self.frames.saturating_sub(self.consumed_frames)
    }

    fn is_depleted(&self) -> bool {
        self.consumed_frames >= self.frames
    }

    fn append_prefix_to(
        &mut self,
        frames: u32,
        bytes_per_frame: usize,
        sample_rate: u32,
        packet_data: &mut Vec<u8>,
        packet_meta: &mut Option<PendingMetadata>,
    ) -> AudioResult<()> {
        if frames == 0 {
            return Ok(());
        }

        let start = (self.consumed_frames as usize)
            .checked_mul(bytes_per_frame)
            .ok_or(AudioError::BufferOverflow)?;
        let byte_count = (frames as usize)
            .checked_mul(bytes_per_frame)
            .ok_or(AudioError::BufferOverflow)?;
        let end = start
            .checked_add(byte_count)
            .ok_or(AudioError::BufferOverflow)?;
        if end > self.data.len() {
            return Err(AudioError::BufferOverflow);
        }

        packet_data.extend_from_slice(&self.data[start..end]);
        let slice_meta = self.slice_end_metadata(frames, sample_rate)?;
        merge_pending_meta(packet_meta, slice_meta);
        self.consumed_frames = self
            .consumed_frames
            .checked_add(frames)
            .ok_or(AudioError::BufferOverflow)?;
        Ok(())
    }

    fn slice_end_metadata(
        &self,
        consumed_slice_frames: u32,
        sample_rate: u32,
    ) -> AudioResult<PendingMetadata> {
        let slice_end_frame = self
            .consumed_frames
            .checked_add(consumed_slice_frames)
            .ok_or(AudioError::BufferOverflow)?;
        if slice_end_frame > self.frames {
            return Err(AudioError::BufferOverflow);
        }

        let tail_frames = self.frames.saturating_sub(slice_end_frame);
        let tail_duration = frames_to_duration(tail_frames, sample_rate);
        let tail_100ns = frames_to_100ns(tail_frames, sample_rate);
        let tail_frames_u64 = u64::from(tail_frames);

        Ok(PendingMetadata {
            capture_time: self
                .metadata
                .capture_time
                .map(|end| end.checked_sub(tail_duration).unwrap_or(end)),
            qpc_position_100ns: self
                .metadata
                .qpc_position_100ns
                .map(|end| end.saturating_sub(tail_100ns)),
            device_position_frames: self
                .metadata
                .device_position_frames
                .map(|end| end.saturating_sub(tail_frames_u64)),
            // WASAPI discontinuity marks the first sample after a gap, so it
            // only applies to the first slice emitted from this chunk.
            discontinuity: self.metadata.discontinuity && self.consumed_frames == 0,
            is_silent: self.metadata.is_silent,
        })
    }
}

struct PacketAccumulator {
    source: AudioSourceKind,
    format: AudioFormat,
    target_frames: u32,
    bytes_per_frame: usize,
    chunks: VecDeque<BufferedChunk>,
    buffered_frames: u32,
}

impl PacketAccumulator {
    fn new(
        source: AudioSourceKind,
        format: AudioFormat,
        packet_duration: std::time::Duration,
    ) -> AudioResult<Self> {
        let bytes_per_frame = format.bytes_per_frame()?;
        let mut target_frames = (format.sample_rate as u128)
            .checked_mul(packet_duration.as_nanos())
            .and_then(|value| value.checked_div(1_000_000_000))
            .ok_or(AudioError::BufferOverflow)? as u32;
        if target_frames == 0 {
            target_frames = 1;
        }

        Ok(Self {
            source,
            format,
            target_frames,
            bytes_per_frame,
            chunks: VecDeque::new(),
            buffered_frames: 0,
        })
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.buffered_frames = 0;
    }

    fn push_chunk(
        &mut self,
        bytes: &[u8],
        frames: u32,
        meta: PendingMetadata,
        sequence: &mut u64,
    ) -> AudioResult<Vec<AudioPacket>> {
        if frames == 0 || bytes.is_empty() {
            return Ok(Vec::new());
        }

        let expected_len = self
            .bytes_per_frame
            .checked_mul(frames as usize)
            .ok_or(AudioError::BufferOverflow)?;
        if bytes.len() != expected_len {
            return Err(AudioError::BufferOverflow);
        }

        self.chunks.push_back(BufferedChunk {
            data: bytes.to_vec(),
            frames,
            consumed_frames: 0,
            metadata: meta,
        });
        self.buffered_frames = self
            .buffered_frames
            .checked_add(frames)
            .ok_or(AudioError::BufferOverflow)?;

        let mut output = Vec::new();
        while self.buffered_frames >= self.target_frames {
            output.push(self.build_packet(sequence)?);
        }

        Ok(output)
    }

    fn build_packet(&mut self, sequence: &mut u64) -> AudioResult<AudioPacket> {
        let packet_byte_count = self
            .bytes_per_frame
            .checked_mul(self.target_frames as usize)
            .ok_or(AudioError::BufferOverflow)?;

        let mut data = Vec::with_capacity(packet_byte_count);
        let mut packet_meta: Option<PendingMetadata> = None;
        let mut remaining_frames = self.target_frames;

        while remaining_frames > 0 {
            let drop_chunk = {
                let chunk = self.chunks.front_mut().ok_or(AudioError::BufferOverflow)?;
                let available = chunk.remaining_frames();
                if available == 0 {
                    true
                } else {
                    let take = available.min(remaining_frames);
                    chunk.append_prefix_to(
                        take,
                        self.bytes_per_frame,
                        self.format.sample_rate,
                        &mut data,
                        &mut packet_meta,
                    )?;
                    remaining_frames = remaining_frames.saturating_sub(take);
                    chunk.is_depleted()
                }
            };
            if drop_chunk {
                let _ = self.chunks.pop_front();
            }
        }

        if data.len() != packet_byte_count {
            return Err(AudioError::BufferOverflow);
        }

        self.buffered_frames = self
            .buffered_frames
            .checked_sub(self.target_frames)
            .ok_or(AudioError::BufferOverflow)?;
        *sequence = sequence.wrapping_add(1);

        let pending = packet_meta.unwrap_or_else(|| PendingMetadata {
            is_silent: true,
            ..Default::default()
        });

        Ok(AudioPacket {
            source: self.source,
            format: self.format,
            frames: self.target_frames,
            data,
            metadata: AudioPacketMetadata {
                capture_time: pending.capture_time,
                qpc_position_100ns: pending.qpc_position_100ns,
                device_position_frames: pending.device_position_frames,
                discontinuity: pending.discontinuity,
                is_silent: pending.is_silent,
                sequence: *sequence,
            },
        })
    }
}

fn merge_pending_meta(slot: &mut Option<PendingMetadata>, incoming: PendingMetadata) {
    match slot {
        Some(existing) => {
            if let Some(capture_time) = incoming.capture_time {
                existing.capture_time = Some(
                    existing
                        .capture_time
                        .map_or(capture_time, |current| current.max(capture_time)),
                );
            }

            if let Some(qpc) = incoming.qpc_position_100ns {
                existing.qpc_position_100ns = Some(
                    existing
                        .qpc_position_100ns
                        .map_or(qpc, |current| current.max(qpc)),
                );
            }

            if let Some(position) = incoming.device_position_frames {
                existing.device_position_frames = Some(
                    existing
                        .device_position_frames
                        .map_or(position, |current| current.max(position)),
                );
            }

            existing.discontinuity |= incoming.discontinuity;
            existing.is_silent &= incoming.is_silent;
        }
        None => {
            *slot = Some(incoming);
        }
    }
}

pub(crate) struct WasapiSource {
    kind: AudioSourceKind,
    flow: DeviceFlow,
    selector: DeviceSelector,
    config: SourceConfig,
    enumerator: IMMDeviceEnumerator,
    event: EventHandle,
    runtime: SourceRuntime,
    converter: AudioConverter,
    accumulator: PacketAccumulator,
    sequence: u64,
    silence_scratch: Vec<u8>,
}

impl WasapiSource {
    pub fn new(
        kind: AudioSourceKind,
        config: SourceConfig,
        enumerator: IMMDeviceEnumerator,
    ) -> AudioResult<Self> {
        let flow = match kind {
            AudioSourceKind::System => DeviceFlow::Render,
            AudioSourceKind::Microphone => DeviceFlow::Capture,
        };

        let event = EventHandle::new_auto_reset(false)?;
        let selector = config.device.clone();
        let (runtime, converter) =
            init_runtime(kind, flow, &selector, &config, &enumerator, event.raw())?;

        let accumulator =
            PacketAccumulator::new(kind, config.output_format, config.packet_duration)?;

        Ok(Self {
            kind,
            flow,
            selector,
            config,
            enumerator,
            event,
            runtime,
            converter,
            accumulator,
            sequence: 0,
            silence_scratch: Vec::new(),
        })
    }

    pub fn current_device_id(&self) -> &str {
        &self.runtime.device_id
    }

    pub fn event_handle(&self) -> windows::Win32::Foundation::HANDLE {
        self.event.raw()
    }

    pub fn restart(&mut self) -> AudioResult<(Option<String>, String)> {
        let old_id = Some(self.runtime.device_id.clone());
        let _ = unsafe { self.runtime.audio_client.Stop() };

        self.accumulator.clear();

        let (runtime, converter) = init_runtime(
            self.kind,
            self.flow,
            &self.selector,
            &self.config,
            &self.enumerator,
            self.event.raw(),
        )?;
        self.runtime = runtime;
        self.converter = converter;

        Ok((old_id, self.runtime.device_id.clone()))
    }

    pub fn drain_packets(&mut self) -> AudioResult<Vec<AudioPacket>> {
        let mut packets = Vec::new();
        loop {
            let next = unsafe { self.runtime.capture_client.GetNextPacketSize() }
                .map_err(|err| map_hresult(err.code(), "IAudioCaptureClient::GetNextPacketSize"))?;

            if next == 0 {
                break;
            }

            let mut data_ptr = ptr::null_mut::<u8>();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut device_position = 0u64;
            let mut qpc_position = 0u64;

            unsafe {
                self.runtime.capture_client.GetBuffer(
                    &mut data_ptr,
                    &mut frames,
                    &mut flags,
                    Some(&mut device_position),
                    Some(&mut qpc_position),
                )
            }
            .map_err(|err| map_hresult(err.code(), "IAudioCaptureClient::GetBuffer"))?;

            let process_result = (|| {
                let native_bytes_per_frame = self.runtime.native_format.bytes_per_frame()?;
                let byte_count = native_bytes_per_frame
                    .checked_mul(frames as usize)
                    .ok_or(AudioError::BufferOverflow)?;

                let is_silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let discontinuity = (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0;

                let converted = if is_silent {
                    self.silence_scratch.resize(byte_count, 0);
                    self.converter.convert_chunk(&self.silence_scratch, frames)
                } else {
                    if data_ptr.is_null() {
                        return Err(AudioError::platform(anyhow::anyhow!(
                            "WASAPI returned null audio buffer pointer"
                        )));
                    }
                    let input = unsafe { slice::from_raw_parts(data_ptr, byte_count) };
                    self.converter.convert_chunk(input, frames)
                }?;

                if converted.is_empty() {
                    return Ok(Vec::new());
                }

                let out_bpf = self.config.output_format.bytes_per_frame()?;
                if converted.len() % out_bpf != 0 {
                    return Err(AudioError::BufferOverflow);
                }
                let out_frames = (converted.len() / out_bpf) as u32;

                self.accumulator.push_chunk(
                    &converted,
                    out_frames,
                    PendingMetadata {
                        capture_time: Some(Instant::now()),
                        qpc_position_100ns: Some(qpc_position as i64),
                        device_position_frames: Some(device_position),
                        discontinuity,
                        is_silent,
                    },
                    &mut self.sequence,
                )
            })();

            unsafe {
                self.runtime
                    .capture_client
                    .ReleaseBuffer(frames)
                    .map_err(|err| map_hresult(err.code(), "IAudioCaptureClient::ReleaseBuffer"))?;
            }

            packets.extend(process_result?);
        }

        Ok(packets)
    }
}

impl Drop for WasapiSource {
    fn drop(&mut self) {
        unsafe {
            let _ = self.runtime.audio_client.Stop();
        }
    }
}

fn init_runtime(
    kind: AudioSourceKind,
    flow: DeviceFlow,
    selector: &DeviceSelector,
    config: &SourceConfig,
    enumerator: &IMMDeviceEnumerator,
    event_handle: windows::Win32::Foundation::HANDLE,
) -> AudioResult<(SourceRuntime, AudioConverter)> {
    let device = device_enum::resolve_device(enumerator, selector, flow)?;
    let device_id = device_enum::device_id(&device)?;

    let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|err| map_hresult(err.code(), "IMMDevice::Activate(IAudioClient)"))?;

    let (selected_format, native_format) =
        select_stream_format(&audio_client, config.output_format)?;

    let duration_hns = duration_to_hns(config.packet_duration)?;
    let mut stream_flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_NOPERSIST
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

    if kind == AudioSourceKind::System {
        stream_flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }

    let format_ptr = selected_format.as_ptr() as *const WAVEFORMATEX;

    unsafe {
        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            stream_flags,
            duration_hns,
            0,
            format_ptr,
            None,
        )
    }
    .map_err(|err| map_hresult(err.code(), "IAudioClient::Initialize"))?;

    unsafe { audio_client.SetEventHandle(event_handle) }
        .map_err(|err| map_hresult(err.code(), "IAudioClient::SetEventHandle"))?;

    let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService() }
        .map_err(|err| map_hresult(err.code(), "IAudioClient::GetService(IAudioCaptureClient)"))?;

    unsafe { audio_client.Start() }
        .map_err(|err| map_hresult(err.code(), "IAudioClient::Start"))?;

    let converter = AudioConverter::new(native_format, config.output_format)?;

    Ok((
        SourceRuntime {
            device_id,
            audio_client,
            capture_client,
            native_format,
        },
        converter,
    ))
}

fn duration_to_hns(duration: std::time::Duration) -> AudioResult<i64> {
    let nanos = duration.as_nanos();
    let hns = nanos.checked_div(100).ok_or(AudioError::BufferOverflow)?;

    if hns > i64::MAX as u128 {
        return Err(AudioError::BufferOverflow);
    }

    Ok(hns as i64)
}

fn select_stream_format(
    audio_client: &IAudioClient,
    output: AudioFormat,
) -> AudioResult<(Vec<u8>, NativeAudioFormat)> {
    let requested = build_requested_wave_format(output)?;
    let requested_ptr = requested.as_ptr() as *const WAVEFORMATEX;

    let mut closest_ptr: *mut WAVEFORMATEX = ptr::null_mut();
    let support_hr = unsafe {
        audio_client.IsFormatSupported(
            AUDCLNT_SHAREMODE_SHARED,
            requested_ptr,
            Some(&mut closest_ptr),
        )
    };

    let selected = if support_hr == S_OK {
        requested
    } else if support_hr.is_ok() && !closest_ptr.is_null() {
        copy_wave_format(closest_ptr)?
    } else if support_hr == AUDCLNT_E_UNSUPPORTED_FORMAT {
        let mix_ptr = unsafe { audio_client.GetMixFormat() }
            .map_err(|err| map_hresult(err.code(), "IAudioClient::GetMixFormat"))?;
        let mix = copy_wave_format(mix_ptr);
        unsafe {
            CoTaskMemFree(Some(mix_ptr as *const _));
        }
        mix?
    } else {
        if !closest_ptr.is_null() {
            unsafe {
                CoTaskMemFree(Some(closest_ptr as *const _));
            }
        }
        return Err(map_hresult(support_hr, "IAudioClient::IsFormatSupported"));
    };

    if !closest_ptr.is_null() {
        unsafe {
            CoTaskMemFree(Some(closest_ptr as *const _));
        }
    }

    let native = parse_native_format(selected.as_ptr() as *const WAVEFORMATEX)?;
    Ok((selected, native))
}

fn copy_wave_format(ptr: *const WAVEFORMATEX) -> AudioResult<Vec<u8>> {
    if ptr.is_null() {
        return Err(AudioError::UnsupportedFormat(
            "WASAPI returned null format pointer".into(),
        ));
    }

    let base = unsafe { *ptr };
    let total = size_of::<WAVEFORMATEX>()
        .checked_add(base.cbSize as usize)
        .ok_or(AudioError::BufferOverflow)?;
    let bytes = unsafe { slice::from_raw_parts(ptr as *const u8, total) };
    Ok(bytes.to_vec())
}

fn build_requested_wave_format(output: AudioFormat) -> AudioResult<Vec<u8>> {
    let bits_per_sample: u16 = match output.sample_format {
        AudioSampleFormat::F32 => 32,
        AudioSampleFormat::I16 => 16,
    };

    let block_align = u32::from(output.channels)
        .checked_mul(u32::from(bits_per_sample))
        .and_then(|v| v.checked_div(8))
        .ok_or(AudioError::BufferOverflow)? as u16;

    let avg_bytes_per_sec = output
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or(AudioError::BufferOverflow)?;

    let extensible = WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: output.channels,
            nSamplesPerSec: output.sample_rate,
            nAvgBytesPerSec: avg_bytes_per_sec,
            nBlockAlign: block_align,
            wBitsPerSample: bits_per_sample,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits_per_sample,
        },
        dwChannelMask: channel_mask(output.channels),
        SubFormat: match output.sample_format {
            AudioSampleFormat::F32 => KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            AudioSampleFormat::I16 => KSDATAFORMAT_SUBTYPE_PCM,
        },
    };

    let bytes = unsafe {
        slice::from_raw_parts(
            &extensible as *const WAVEFORMATEXTENSIBLE as *const u8,
            size_of_val(&extensible),
        )
    };

    Ok(bytes.to_vec())
}

fn channel_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,  // SPEAKER_FRONT_CENTER
        2 => 0x3,  // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        6 => 0x3f, // 5.1 standard mask
        _ => 0,
    }
}

fn parse_native_format(ptr: *const WAVEFORMATEX) -> AudioResult<NativeAudioFormat> {
    if ptr.is_null() {
        return Err(AudioError::UnsupportedFormat(
            "WASAPI reported null format".into(),
        ));
    }

    let wf = unsafe { ptr.read_unaligned() };
    let sample_rate = unsafe { std::ptr::addr_of!(wf.nSamplesPerSec).read_unaligned() };
    let channels = unsafe { std::ptr::addr_of!(wf.nChannels).read_unaligned() };
    let format_tag = unsafe { std::ptr::addr_of!(wf.wFormatTag).read_unaligned() } as u32;
    let bits_per_sample = unsafe { std::ptr::addr_of!(wf.wBitsPerSample).read_unaligned() };
    let cb_size = unsafe { std::ptr::addr_of!(wf.cbSize).read_unaligned() };

    let sample_format = match format_tag {
        tag if tag == WAVE_FORMAT_PCM => match bits_per_sample {
            16 => NativeSampleFormat::I16,
            32 => NativeSampleFormat::I32,
            bits => {
                return Err(AudioError::UnsupportedFormat(format!(
                    "unsupported PCM bit depth: {bits}"
                )));
            }
        },
        tag if tag == u32::from(IEEE_FLOAT_FORMAT_TAG) => {
            if bits_per_sample != 32 {
                return Err(AudioError::UnsupportedFormat(format!(
                    "unsupported IEEE float bit depth: {}",
                    bits_per_sample
                )));
            }
            NativeSampleFormat::F32
        }
        tag if tag == WAVE_FORMAT_EXTENSIBLE => {
            if (cb_size as usize) < (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>())
            {
                return Err(AudioError::UnsupportedFormat(
                    "malformed WAVE_FORMAT_EXTENSIBLE format".into(),
                ));
            }

            let extensible = unsafe { (ptr as *const WAVEFORMATEXTENSIBLE).read_unaligned() };
            let subformat = unsafe { std::ptr::addr_of!(extensible.SubFormat).read_unaligned() };

            if subformat == KSDATAFORMAT_SUBTYPE_PCM {
                match bits_per_sample {
                    16 => NativeSampleFormat::I16,
                    32 => NativeSampleFormat::I32,
                    bits => {
                        return Err(AudioError::UnsupportedFormat(format!(
                            "unsupported extensible PCM bit depth: {bits}"
                        )));
                    }
                }
            } else if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                if bits_per_sample != 32 {
                    return Err(AudioError::UnsupportedFormat(format!(
                        "unsupported extensible float bit depth: {}",
                        bits_per_sample
                    )));
                }
                NativeSampleFormat::F32
            } else {
                return Err(AudioError::UnsupportedFormat(format!(
                    "unsupported extensible subformat: {:?}",
                    subformat
                )));
            }
        }
        other => {
            return Err(AudioError::UnsupportedFormat(format!(
                "unsupported wave format tag: {other}"
            )));
        }
    };

    Ok(NativeAudioFormat {
        sample_rate,
        channels,
        sample_format,
    })
}
