//! PCM helpers kept for the audio crate's tests and the smoke tooling. The
//! Unity side builds its quad frames itself; the session only needs to push
//! interleaved samples and resample when the clip's rate differs from the
//! endpoint's.

pub const RATE: u32 = 48_000;

#[derive(Debug, Clone, PartialEq)]
pub struct PcmAudio {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl PcmAudio {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }
}

pub fn load_wav(path: &std::path::Path) -> Result<PcmAudio, String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        hound::SampleFormat::Int => {
            if !(1..=32).contains(&spec.bits_per_sample) {
                return Err(format!("unsupported bit depth: {}", spec.bits_per_sample));
            }
            let scale = (1_u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / scale))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?,
    };
    Ok(PcmAudio {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        samples,
    })
}

/// Places a stereo source on two channels of a four-channel block.
/// Channels 1/2 (zero-based 0/1) are the pad's speaker/headset, 3/4 (2/3) drive
/// the left/right voice coils.
pub fn map_stereo_to_quad(
    source: &PcmAudio,
    left_channel: usize,
    right_channel: usize,
) -> PcmAudio {
    assert_eq!(source.channels, 2, "source must be stereo");
    assert!(left_channel < 4 && right_channel < 4 && left_channel != right_channel);
    let mut samples = Vec::with_capacity(source.frames() * 4);
    for frame in source.samples.chunks_exact(2) {
        let mut quad = [0.0f32; 4];
        quad[left_channel] = frame[0];
        quad[right_channel] = frame[1];
        samples.extend_from_slice(&quad);
    }
    PcmAudio {
        sample_rate: source.sample_rate,
        channels: 4,
        samples,
    }
}

/// Linear resampler into a caller-owned buffer (reused across calls, so the
/// steady state does not allocate).
pub fn resample_stereo(source: &[f32], from: u32, to: u32, out: &mut Vec<f32>) {
    out.clear();
    if from == 0 || to == 0 || source.len() < 2 {
        return;
    }
    if from == to {
        out.extend_from_slice(source);
        return;
    }
    let source_frames = source.len() / 2;
    let target_frames = (source_frames as u64 * to as u64 / from as u64) as usize;
    out.reserve(target_frames * 2);
    for frame in 0..target_frames {
        // Linear interpolation between neighbours.
        let position = frame as f64 * from as f64 / to as f64;
        let index = position.floor() as usize;
        let fraction = (position - index as f64) as f32;
        let next = (index + 1).min(source_frames - 1);
        for channel in 0..2 {
            let a = source[index * 2 + channel];
            let b = source[next * 2 + channel];
            out.push(a + (b - a) * fraction);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_goes_to_the_actuators() {
        let source = PcmAudio {
            sample_rate: RATE,
            channels: 2,
            samples: vec![0.5, -0.5, 1.0, 0.0],
        };
        let quad = map_stereo_to_quad(&source, 2, 3);
        assert_eq!(quad.channels, 4);
        assert_eq!(quad.samples, vec![0.0, 0.0, 0.5, -0.5, 0.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn resampling_keeps_duration() {
        let source: Vec<f32> = (0..441).flat_map(|i| [i as f32, 0.0]).collect();
        let mut out = Vec::new();
        resample_stereo(&source, 44_100, 48_000, &mut out);
        assert_eq!(out.len() / 2, 480);
        // 44.1 kHz held for 10 ms becomes 48 kHz for 10 ms.
        assert_eq!(out[0], 0.0);
        assert!(out.iter().all(|v| v.is_finite()));
        // Same rate is a straight copy.
        resample_stereo(&source, 44_100, 44_100, &mut out);
        assert_eq!(out, source);
    }
}
