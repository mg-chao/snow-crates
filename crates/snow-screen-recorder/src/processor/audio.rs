use snow_audio_recorder::AudioSourceKind;

use crate::error::Result;
use crate::recording::PcmTrackWriter;
use crate::timeline::PauseTimeline;

/// Processes audio packets: PCM alignment and writing.
///
/// Routes audio packets to the correct track writer (system or microphone)
/// based on the `AudioSourceKind` field. Tracks whether any audio has been
/// successfully written to each track.
pub(crate) struct AudioProcessor {
    system_writer: Option<PcmTrackWriter>,
    mic_writer: Option<PcmTrackWriter>,
    recorded_system: bool,
    recorded_mic: bool,
}

impl AudioProcessor {
    /// Create a new `AudioProcessor` with optional system and microphone
    /// track writers. Pass `None` for a source that is disabled.
    pub(crate) fn new(
        system_writer: Option<PcmTrackWriter>,
        mic_writer: Option<PcmTrackWriter>,
    ) -> Self {
        Self {
            system_writer,
            mic_writer,
            recorded_system: false,
            recorded_mic: false,
        }
    }

    /// Write an audio packet to the correct track based on `source`.
    ///
    /// Returns `Ok(0)` without error when the corresponding writer is
    /// `None` (source disabled). Sets `recorded_system` / `recorded_mic`
    /// flags on successful writes (appended > 0).
    pub(crate) fn write_packet(
        &mut self,
        source: AudioSourceKind,
        packet: &snow_audio_recorder::AudioPacket,
        bytes: &[u8],
        timeline: &PauseTimeline,
    ) -> Result<u64> {
        match source {
            AudioSourceKind::System => {
                let Some(writer) = self.system_writer.as_mut() else {
                    return Ok(0);
                };
                let appended = writer.write_packet(packet, bytes, timeline)?;
                if appended > 0 {
                    self.recorded_system = true;
                }
                Ok(appended)
            }
            AudioSourceKind::Microphone => {
                let Some(writer) = self.mic_writer.as_mut() else {
                    return Ok(0);
                };
                let appended = writer.write_packet(packet, bytes, timeline)?;
                if appended > 0 {
                    self.recorded_mic = true;
                }
                Ok(appended)
            }
        }
    }

    /// Write silence frames for a dropped packet to the correct track.
    ///
    /// Returns `Ok(())` without error when the corresponding writer is
    /// `None` (source disabled). Sets recording flags when frames > 0.
    pub(crate) fn write_silence(
        &mut self,
        source: AudioSourceKind,
        frames: u64,
    ) -> Result<()> {
        match source {
            AudioSourceKind::System => {
                if let Some(writer) = self.system_writer.as_mut() {
                    writer.append_silence_frames(frames)?;
                    if frames > 0 {
                        self.recorded_system = true;
                    }
                }
            }
            AudioSourceKind::Microphone => {
                if let Some(writer) = self.mic_writer.as_mut() {
                    writer.append_silence_frames(frames)?;
                    if frames > 0 {
                        self.recorded_mic = true;
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether any system audio has been successfully written.
    pub(crate) fn recorded_system(&self) -> bool {
        self.recorded_system
    }

    /// Whether any microphone audio has been successfully written.
    pub(crate) fn recorded_mic(&self) -> bool {
        self.recorded_mic
    }

    /// Flush and close both track writers.
    pub(crate) fn finish(self) -> Result<()> {
        if let Some(writer) = self.system_writer {
            writer.finish()?;
        }
        if let Some(writer) = self.mic_writer {
            writer.finish()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use snow_audio_recorder::format::{AudioFormat, AudioSampleFormat};
    use snow_audio_recorder::{AudioPacket, AudioPacketMetadata, AudioSourceKind};
    use std::time::Instant;
    use tempfile::TempDir;

    /// Create a `PcmTrackWriter` backed by a temp file.
    fn temp_writer(dir: &TempDir, name: &str) -> PcmTrackWriter {
        let path = dir.path().join(name);
        PcmTrackWriter::create(&path, 48_000, 2, Instant::now())
            .expect("temp writer creation should succeed")
    }

    /// Build a minimal valid `AudioPacket` with the given source and
    /// `frames` of stereo I16 silence.
    fn make_packet(source: AudioSourceKind, frames: u32) -> (AudioPacket, Vec<u8>) {
        let format = AudioFormat::new(48_000, 2, AudioSampleFormat::I16);
        let byte_count = (frames as usize) * 2 * 2; // 2 channels * 2 bytes per sample
        let bytes = vec![0u8; byte_count];
        let packet = AudioPacket {
            source,
            format,
            frames,
            data: bytes.clone(),
            metadata: AudioPacketMetadata::default(),
        };
        (packet, bytes)
    }

    /// Strategy that produces an arbitrary `AudioSourceKind`.
    fn arb_source() -> impl Strategy<Value = AudioSourceKind> {
        prop_oneof![
            Just(AudioSourceKind::System),
            Just(AudioSourceKind::Microphone),
        ]
    }

    // **Validates: Requirements 4.2, 4.3, 4.4**
    //
    // Property 4: Audio track isolation and recording flags
    //
    // For any audio source kind and non-zero frame count, writing a
    // packet routes to the correct writer and sets only the
    // corresponding recording flag.
    proptest! {
        #[test]
        fn prop_track_isolation_with_both_writers(
            source in arb_source(),
            frames in 1u32..=960,
        ) {
            let dir = TempDir::new().unwrap();
            let sys_writer = temp_writer(&dir, "sys.pcm");
            let mic_writer = temp_writer(&dir, "mic.pcm");
            let mut proc = AudioProcessor::new(Some(sys_writer), Some(mic_writer));
            let timeline = PauseTimeline::new(Instant::now());

            let (packet, bytes) = make_packet(source, frames);
            let appended = proc.write_packet(source, &packet, &bytes, &timeline).unwrap();

            // With valid non-empty data, appended should be > 0.
            prop_assert!(appended > 0, "expected appended > 0, got {appended}");

            match source {
                AudioSourceKind::System => {
                    prop_assert!(proc.recorded_system(),
                        "recorded_system should be true after System write");
                    prop_assert!(!proc.recorded_mic(),
                        "recorded_mic should remain false after System write");
                }
                AudioSourceKind::Microphone => {
                    prop_assert!(proc.recorded_mic(),
                        "recorded_mic should be true after Microphone write");
                    prop_assert!(!proc.recorded_system(),
                        "recorded_system should remain false after Microphone write");
                }
            }
        }

        /// **Validates: Requirements 4.2, 4.5**
        ///
        /// When the corresponding writer is `None`, `write_packet`
        /// returns `Ok(0)` and no recording flag is set.
        #[test]
        fn prop_none_writer_returns_zero_and_no_flags(
            source in arb_source(),
            frames in 1u32..=960,
        ) {
            // Both writers are None — source is disabled.
            let mut proc = AudioProcessor::new(None, None);
            let timeline = PauseTimeline::new(Instant::now());

            let (packet, bytes) = make_packet(source, frames);
            let appended = proc.write_packet(source, &packet, &bytes, &timeline).unwrap();

            prop_assert_eq!(appended, 0, "expected Ok(0) when writer is None");
            prop_assert!(!proc.recorded_system(),
                "recorded_system should remain false when writer is None");
            prop_assert!(!proc.recorded_mic(),
                "recorded_mic should remain false when writer is None");
        }

        /// **Validates: Requirements 4.2, 4.3, 4.4**
        ///
        /// When only one writer is present, writing to the other source
        /// returns `Ok(0)` and does not affect the present writer's flag.
        #[test]
        fn prop_partial_writer_isolation(
            frames in 1u32..=960,
        ) {
            let dir = TempDir::new().unwrap();
            let timeline = PauseTimeline::new(Instant::now());

            // Only system writer present — mic writes should be no-ops.
            {
                let sys_writer = temp_writer(&dir, "sys_only.pcm");
                let mut proc = AudioProcessor::new(Some(sys_writer), None);

                let (packet, bytes) = make_packet(AudioSourceKind::Microphone, frames);
                let appended = proc.write_packet(AudioSourceKind::Microphone, &packet, &bytes, &timeline).unwrap();
                prop_assert_eq!(appended, 0);
                prop_assert!(!proc.recorded_mic());
                prop_assert!(!proc.recorded_system());
            }

            // Only mic writer present — system writes should be no-ops.
            {
                let mic_writer = temp_writer(&dir, "mic_only.pcm");
                let mut proc = AudioProcessor::new(None, Some(mic_writer));

                let (packet, bytes) = make_packet(AudioSourceKind::System, frames);
                let appended = proc.write_packet(AudioSourceKind::System, &packet, &bytes, &timeline).unwrap();
                prop_assert_eq!(appended, 0);
                prop_assert!(!proc.recorded_system());
                prop_assert!(!proc.recorded_mic());
            }
        }
    }

    // ── Unit tests with synthetic data ──────────────────────────

    #[test]
    fn system_write_sets_recorded_system_flag() {
        let dir = TempDir::new().unwrap();
        let sys_writer = temp_writer(&dir, "sys.pcm");
        let mut proc = AudioProcessor::new(Some(sys_writer), None);
        let timeline = PauseTimeline::new(Instant::now());

        assert!(!proc.recorded_system());
        let (packet, bytes) = make_packet(AudioSourceKind::System, 480);
        proc.write_packet(AudioSourceKind::System, &packet, &bytes, &timeline).unwrap();
        assert!(proc.recorded_system());
        assert!(!proc.recorded_mic());
    }

    #[test]
    fn mic_write_sets_recorded_mic_flag() {
        let dir = TempDir::new().unwrap();
        let mic_writer = temp_writer(&dir, "mic.pcm");
        let mut proc = AudioProcessor::new(None, Some(mic_writer));
        let timeline = PauseTimeline::new(Instant::now());

        assert!(!proc.recorded_mic());
        let (packet, bytes) = make_packet(AudioSourceKind::Microphone, 480);
        proc.write_packet(AudioSourceKind::Microphone, &packet, &bytes, &timeline).unwrap();
        assert!(proc.recorded_mic());
        assert!(!proc.recorded_system());
    }

    #[test]
    fn write_silence_to_system_sets_flag() {
        let dir = TempDir::new().unwrap();
        let sys_writer = temp_writer(&dir, "sys_silence.pcm");
        let mut proc = AudioProcessor::new(Some(sys_writer), None);

        assert!(!proc.recorded_system());
        proc.write_silence(AudioSourceKind::System, 100).unwrap();
        assert!(proc.recorded_system());
    }

    #[test]
    fn write_silence_to_mic_sets_flag() {
        let dir = TempDir::new().unwrap();
        let mic_writer = temp_writer(&dir, "mic_silence.pcm");
        let mut proc = AudioProcessor::new(None, Some(mic_writer));

        assert!(!proc.recorded_mic());
        proc.write_silence(AudioSourceKind::Microphone, 100).unwrap();
        assert!(proc.recorded_mic());
    }

    #[test]
    fn write_silence_zero_frames_does_not_set_flag() {
        let dir = TempDir::new().unwrap();
        let sys_writer = temp_writer(&dir, "sys_zero.pcm");
        let mut proc = AudioProcessor::new(Some(sys_writer), None);

        proc.write_silence(AudioSourceKind::System, 0).unwrap();
        assert!(!proc.recorded_system());
    }

    #[test]
    fn write_silence_to_none_writer_is_noop() {
        let mut proc = AudioProcessor::new(None, None);
        proc.write_silence(AudioSourceKind::System, 100).unwrap();
        proc.write_silence(AudioSourceKind::Microphone, 100).unwrap();
        assert!(!proc.recorded_system());
        assert!(!proc.recorded_mic());
    }

    #[test]
    fn finish_succeeds_with_no_writers() {
        let proc = AudioProcessor::new(None, None);
        proc.finish().unwrap();
    }

    #[test]
    fn finish_succeeds_with_writers() {
        let dir = TempDir::new().unwrap();
        let sys_writer = temp_writer(&dir, "sys_finish.pcm");
        let mic_writer = temp_writer(&dir, "mic_finish.pcm");
        let proc = AudioProcessor::new(Some(sys_writer), Some(mic_writer));
        proc.finish().unwrap();
    }
}
