//! Rumble shapes as waveforms for the voice-coil actuators.
//!
//! The pad has no rotating motors on USB: it is a four-channel 48 kHz audio
//! device whose channels 3 and 4 drive the left and right actuator. A shape is
//! a list of `(heavy, light)` levels held for a while; each level becomes a tone
//! (80 Hz for the heavy part, 200 Hz for the light one) with short ramps between
//! steps and to silence, so a level change does not click.

use std::f32::consts::TAU;

/// The frequencies the heavy and light parts of a vibration play at.
pub const HEAVY_HZ: f32 = 80.0;
pub const LIGHT_HZ: f32 = 200.0;
/// Fades between steps.
const RAMP_SECONDS: f32 = 0.004;

/// One step of a shape: levels 0..255 and a duration in milliseconds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RumbleStep {
    pub left: u8,
    pub right: u8,
    pub millis: u16,
}

/// Appends quad frames `[0, 0, left actuator, right actuator]` for `steps`,
/// then a ramp down to silence. `out` is cleared first and reused, so no
/// allocation happens once it is big enough.
pub fn waveform(steps: &[RumbleStep], rate: u32, out: &mut Vec<f32>) {
    out.clear();
    if rate == 0 {
        return;
    }
    let ramp = ((RAMP_SECONDS * rate as f32) as usize).max(1);
    let mut level = [0.0f32; 2];
    let mut time = 0usize;

    for step in steps
        .iter()
        .copied()
        .chain(std::iter::once(RumbleStep::default()))
    {
        let target = [f32::from(step.left) / 255.0, f32::from(step.right) / 255.0];
        let from = level;
        let frames = (u64::from(step.millis) * u64::from(rate) / 1000) as usize;
        for index in 0..frames.max(ramp) {
            let k = (index as f32 / ramp as f32).min(1.0);
            level = [
                from[0] + (target[0] - from[0]) * k,
                from[1] + (target[1] - from[1]) * k,
            ];
            let seconds = time as f32 / rate as f32;
            let heavy = level[0] * (TAU * HEAVY_HZ * seconds).sin();
            let light = level[1] * (TAU * LIGHT_HZ * seconds).sin();
            out.extend_from_slice(&[0.0, 0.0, heavy.clamp(-1.0, 1.0), light.clamp(-1.0, 1.0)]);
            time += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_follows_the_steps() {
        let step = RumbleStep {
            left: 255,
            right: 0,
            millis: 100,
        };
        let mut out = Vec::new();
        waveform(&[step], 48_000, &mut out);
        // 100 ms plus the ramp down.
        assert_eq!(out.len() / 4, 4800 + 192);
        // Starts silent, reaches full strength on the left only, ends silent.
        assert_eq!(out[2], 0.0);
        let peak = out[2..4800 * 4]
            .iter()
            .step_by(4)
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.99, "{peak}");
        // The right actuator stays silent for a heavy-only step.
        assert!(out.iter().skip(3).step_by(4).all(|v| *v == 0.0));
        assert!(out[out.len() - 4..].iter().all(|v| v.abs() < 0.01));
        assert!(out.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn empty_ramps_to_silence() {
        let mut out = Vec::new();
        waveform(&[], 48_000, &mut out);
        assert_eq!(out.len() / 4, 192);
        assert!(out.iter().all(|v| *v == 0.0));
    }
}
