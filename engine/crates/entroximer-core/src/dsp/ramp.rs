//! Gains that change gradually: a sudden jump in gain is heard as a click.

use std::f32::consts::FRAC_PI_2;

use super::CHANNELS;

/// How a gain moves from one value to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curve {
    /// In a straight line; for volume changes.
    Linear,
    /// Along a quarter sine: of two sounds crossfading this way, one fading in and the other
    /// out, the sum keeps a constant power, so the overlap does not dip.
    EqualPower,
}

/// A gain on its way to a target, one step per frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Ramp {
    from: f32,
    to: f32,
    frames: usize,
    elapsed: usize,
    curve: Curve,
}

impl Ramp {
    pub const fn constant(value: f32) -> Self {
        Self {
            from: value,
            to: value,
            frames: 0,
            elapsed: 0,
            curve: Curve::Linear,
        }
    }

    /// Moves from the current value to `target` over `frames` (at once if 0).
    pub fn move_to(&mut self, target: f32, frames: usize, curve: Curve) {
        *self = Self {
            from: self.value(),
            to: target,
            frames,
            elapsed: 0,
            curve,
        };
    }

    pub fn value(&self) -> f32 {
        if self.elapsed >= self.frames {
            return self.to;
        }
        let progress = self.elapsed as f32 / self.frames as f32;
        let shaped = match self.curve {
            Curve::Linear => progress,
            // rising along the sine, falling along the cosine: their squares add up to one
            Curve::EqualPower if self.to >= self.from => (progress * FRAC_PI_2).sin(),
            Curve::EqualPower => 1.0 - (progress * FRAC_PI_2).cos(),
        };
        self.from + (self.to - self.from) * shaped
    }

    pub fn target(&self) -> f32 {
        self.to
    }

    /// Whether the gain has reached its target.
    pub fn is_settled(&self) -> bool {
        self.elapsed >= self.frames
    }

    /// Multiplies interleaved stereo by the gain, advancing it a step per frame.
    pub fn apply(&mut self, interleaved: &mut [f32]) {
        if self.is_settled() {
            if self.to != 1.0 {
                for sample in interleaved.iter_mut() {
                    *sample *= self.to;
                }
            }
            return;
        }
        for frame in interleaved.as_chunks_mut::<CHANNELS>().0 {
            let gain = self.step();
            for sample in frame {
                *sample *= gain;
            }
        }
    }

    /// Adds `source` (interleaved stereo) times the gain to `out`, advancing it a step per frame.
    pub fn apply_add(&mut self, source: &[f32], out: &mut [f32]) {
        if self.is_settled() {
            let gain = self.to;
            for (out, &sample) in out.iter_mut().zip(source) {
                *out += sample * gain;
            }
            return;
        }
        let (out, _) = out.as_chunks_mut::<CHANNELS>();
        let (source, _) = source.as_chunks::<CHANNELS>();
        for (out, source) in out.iter_mut().zip(source) {
            let gain = self.step();
            for (out, &sample) in out.iter_mut().zip(source) {
                *out += sample * gain;
            }
        }
    }

    /// The gain for this frame; then moves on to the next.
    fn step(&mut self) -> f32 {
        let gain = self.value();
        self.elapsed = (self.elapsed + 1).min(self.frames);
        gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaches_its_target_after_the_frames_and_stays() {
        let mut ramp = Ramp::constant(1.0);
        ramp.move_to(0.0, 4, Curve::Linear);
        let mut audio = vec![1.0; 6 * CHANNELS];
        ramp.apply(&mut audio);
        let left: Vec<f32> = audio.iter().step_by(CHANNELS).copied().collect();
        assert_eq!(left, [1.0, 0.75, 0.5, 0.25, 0.0, 0.0]);
        assert!(ramp.is_settled());
        assert_eq!(ramp.value(), 0.0);
    }

    #[test]
    fn a_new_target_starts_from_where_the_gain_is() {
        let mut ramp = Ramp::constant(0.0);
        ramp.move_to(1.0, 10, Curve::Linear);
        ramp.apply(&mut [0.0; 5 * CHANNELS]);
        ramp.move_to(0.0, 5, Curve::Linear);
        assert!((ramp.value() - 0.5).abs() < 1e-6, "{}", ramp.value());
    }

    #[test]
    fn equal_power_fades_keep_the_power_of_their_sum() {
        let (mut fading_in, mut fading_out) = (Ramp::constant(0.0), Ramp::constant(1.0));
        fading_in.move_to(1.0, 100, Curve::EqualPower);
        fading_out.move_to(0.0, 100, Curve::EqualPower);
        for _ in 0..=100 {
            let power = fading_in.step().powi(2) + fading_out.step().powi(2);
            assert!((power - 1.0).abs() < 1e-5, "power {power}");
        }
    }

    #[test]
    fn adds_scaled_audio() {
        let mut ramp = Ramp::constant(0.5);
        let mut out = vec![1.0; 2 * CHANNELS];
        ramp.apply_add(&[2.0; 2 * CHANNELS], &mut out);
        assert_eq!(out, [2.0; 2 * CHANNELS]);
    }
}
