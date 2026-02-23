pub mod mixer;
pub mod mp3_writer;
pub mod opus_encoder;

/// `shine-rs` can panic on the exact minimum i16 sample value in its
/// fixed-point quantization path. Lift that one value by 1 LSB.
#[inline]
pub(crate) fn sanitize_sample_for_mp3(sample: i16) -> i16 {
    if sample == i16::MIN {
        i16::MIN + 1
    } else {
        sample
    }
}

#[inline]
pub(crate) fn sanitize_samples_for_mp3(samples: &mut [i16]) {
    for sample in samples {
        *sample = sanitize_sample_for_mp3(*sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rewrites_i16_minimum() {
        assert_eq!(sanitize_sample_for_mp3(i16::MIN), i16::MIN + 1);
        assert_eq!(sanitize_sample_for_mp3(-1), -1);
        assert_eq!(sanitize_sample_for_mp3(0), 0);
        assert_eq!(sanitize_sample_for_mp3(i16::MAX), i16::MAX);
    }
}
