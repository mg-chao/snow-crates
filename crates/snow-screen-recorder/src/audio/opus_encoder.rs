use crate::error::{Result, ScreenRecorderError};

pub struct OpusEncoderWrapper {
    encoder: *mut unsafe_libopus::OpusEncoder,
    channels: u16,
    frame_size: i32,
    max_packet_bytes: i32,
}

unsafe impl Send for OpusEncoderWrapper {}

impl OpusEncoderWrapper {
    pub fn new(sample_rate_hz: u32, channels: u16, bitrate_kbps: u16) -> Result<Self> {
        let mut err = 0;
        let encoder = unsafe {
            unsafe_libopus::opus_encoder_create(
                sample_rate_hz as i32,
                channels as i32,
                unsafe_libopus::OPUS_APPLICATION_AUDIO,
                &mut err,
            )
        };

        if encoder.is_null() || err != unsafe_libopus::OPUS_OK {
            return Err(ScreenRecorderError::Encode(format!(
                "failed to create opus encoder (err={err})"
            )));
        }

        let bitrate_bps = i32::from(bitrate_kbps) * 1000;
        let ctl_result = unsafe {
            unsafe_libopus::opus_encoder_ctl_impl(
                encoder,
                unsafe_libopus::OPUS_SET_BITRATE_REQUEST,
                unsafe_libopus::varargs!(bitrate_bps),
            )
        };

        if ctl_result != unsafe_libopus::OPUS_OK {
            unsafe { unsafe_libopus::opus_encoder_destroy(encoder) };
            return Err(ScreenRecorderError::Encode(format!(
                "failed to set opus bitrate (err={ctl_result})"
            )));
        }

        Ok(Self {
            encoder,
            channels,
            frame_size: (sample_rate_hz / 50) as i32, // 20ms
            max_packet_bytes: 4000,
        })
    }

    pub fn frame_samples_per_channel(&self) -> usize {
        self.frame_size as usize
    }

    pub fn encode_frame(&mut self, pcm_interleaved: &[i16]) -> Result<Vec<u8>> {
        let expected = self.frame_size as usize * self.channels as usize;
        if pcm_interleaved.len() != expected {
            return Err(ScreenRecorderError::Encode(format!(
                "opus frame expected {} samples, got {}",
                expected,
                pcm_interleaved.len()
            )));
        }

        let mut out = vec![0u8; self.max_packet_bytes as usize];
        let written = unsafe {
            unsafe_libopus::opus_encode(
                self.encoder,
                pcm_interleaved.as_ptr(),
                self.frame_size,
                out.as_mut_ptr(),
                self.max_packet_bytes,
            )
        };

        if written < 0 {
            return Err(ScreenRecorderError::Encode(format!(
                "opus encode failed (err={written})"
            )));
        }

        out.truncate(written as usize);
        Ok(out)
    }
}

impl Drop for OpusEncoderWrapper {
    fn drop(&mut self) {
        if !self.encoder.is_null() {
            unsafe { unsafe_libopus::opus_encoder_destroy(self.encoder) };
            self.encoder = std::ptr::null_mut();
        }
    }
}
