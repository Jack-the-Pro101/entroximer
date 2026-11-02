//! What a deck's audio goes through: its effects, then its volume.

use std::time::Duration;

use super::Effects;
use super::ramp::{Curve, Ramp};

/// How long a volume change takes; long enough not to click.
const VOLUME_RAMP: Duration = Duration::from_millis(20);
const SAMPLE_RATE: f64 = 48_000.0;

#[derive(Debug)]
pub struct Strip {
    effects: Option<Effects>,
    volume: Ramp,
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
        }
    }

    /// Replaces the effects (`None` removes them) from the next render on.
    pub fn set_effects(&mut self, effects: Option<Effects>) {
        self.effects = effects;
    }

    /// Linear gain applied after the effects; it moves there over a few milliseconds.
    pub fn set_volume(&mut self, volume: f32) {
        if volume != self.volume.target() {
            let frames = (VOLUME_RAMP.as_secs_f64() * SAMPLE_RATE) as usize;
            self.volume.move_to(volume, frames, Curve::Linear);
        }
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

    /// Fills `out` (interleaved stereo) from `source` through the effects and the volume.
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
    }
}
