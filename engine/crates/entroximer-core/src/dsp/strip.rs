//! What a deck's audio goes through: its effects, then its volume, then, if asked, a hard clip.

use std::time::Duration;

use super::Effects;
use super::ramp::{Curve, Ramp};

/// How long a volume change takes; long enough not to click.
const VOLUME_RAMP: Duration = Duration::from_millis(20);
const SAMPLE_RATE: f64 = 48_000.0;
/// Where clipping cuts the audio off: just below where the mixer's master limiter starts to
/// lower it (it keeps 10% headroom), so the limiter leaves the clipped audio as it is.
const CLIP_CEILING: f32 = 0.9;

#[derive(Debug)]
pub struct Strip {
    effects: Option<Effects>,
    volume: Ramp,
    clipping: bool,
}

impl Default for Strip {
    fn default() -> Self {
        Self::new()
    }
}

impl Strip {
    pub fn new() -> Self {
        Self {
            effects: None,
            volume: Ramp::constant(1.0),
            clipping: false,
        }
    }

    /// Replaces the effects (`None` removes them) from the next render on, and returns the ones
    /// replaced. They can hold megabytes of delay lines: a caller holding a lock that the audio
    /// thread takes drops them after letting go of it.
    pub fn set_effects(&mut self, effects: Option<Effects>) -> Option<Effects> {
        std::mem::replace(&mut self.effects, effects)
    }

    /// Linear gain applied after the effects; it moves there over a few milliseconds.
    pub fn set_volume(&mut self, volume: f32) {
        if volume != self.volume.target() {
            let frames = (VOLUME_RAMP.as_secs_f64() * SAMPLE_RATE) as usize;
            self.volume.move_to(volume, frames, Curve::Linear);
        }
    }

    /// Whether the audio is hard clipped after the volume, distorting what a volume above 1
    /// pushes past full scale, rather than left for the master limiter to lower.
    pub fn set_clipping(&mut self, clipping: bool) {
        self.clipping = clipping;
    }

    /// How many source frames one output frame takes: 1 unless the effects set a rate.
    pub fn rate(&self) -> f32 {
        self.effects.as_ref().map_or(1.0, Effects::rate)
    }

    /// Clears the effects' state, such as reverb tails.
    pub fn reset(&mut self) {
        if let Some(effects) = &mut self.effects {
            effects.reset();
        }
    }

    /// Fills `out` (interleaved stereo) from `source` through the effects, the volume and the
    /// clipping.
    /// `source` must fill every buffer it is given completely; with a rate, it is asked for
    /// more or fewer frames than `out` holds.
    pub fn render(&mut self, out: &mut [f32], source: impl FnMut(&mut [f32])) {
        match &mut self.effects {
            Some(effects) => effects.render(out, source),
            None => {
                let mut source = source;
                source(out);
            }
        }
        self.volume.apply(out);
        if self.clipping {
            for sample in out {
                *sample = sample.clamp(-CLIP_CEILING, CLIP_CEILING);
            }
        }
    }
}
