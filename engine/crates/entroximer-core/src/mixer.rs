//! A guild's whole output: decks in layers, each with its own effects and volume, summed and kept
//! within full scale.
//!
//! The first layer, [`MAIN`], is the music, which the queue plays into; other layers play clips,
//! sounds outside any queue. A paused layer is not rendered, so its tracks hold their positions
//! while the other layers play on. A layer can *duck* others: lower them while one of its clips is
//! heard, e.g. the music under an announcement.

use std::time::Duration;

use crate::deck::{Deck, DeckEvent, EndReason};
use crate::dsp::{CHANNELS, Curve, Effects, Ramp};
use crate::player::Player;

/// The music's layer.
pub const MAIN: &str = "main";

/// How a layer lowers others while one of its clips is heard.
#[derive(Clone, Debug, PartialEq)]
pub struct Duck {
    /// The layers it lowers, by name.
    pub layers: Vec<String>,
    /// The linear gain they are lowered to.
    pub gain: f32,
    /// Frames lowering them takes.
    pub attack: usize,
    /// Frames bringing them back takes.
    pub release: usize,
    /// Frames they stay lowered after the clips went quiet, so that they do not come back in
    /// every pause between two clips.
    pub hold: usize,
}

#[derive(Debug)]
pub struct Layer {
    name: String,
    pub deck: Deck,
    pub paused: bool,
    pub duck: Option<Duck>,
    /// The gain other layers' ducking puts on this one.
    ducked: Ramp,
    /// How long coming back from being ducked takes: as the layer that ducked it last says.
    release: usize,
    /// Frames this layer keeps ducking for, though its clips went quiet.
    holding: usize,
    /// Whether it lowers the layers its `duck` names.
    ducking: bool,
    /// Whether it was rendered in this block, and so is summed.
    rendered: bool,
    scratch: Vec<f32>,
    /// Whether the last block it rendered was not finite (and so was dropped); reported once.
    broken: bool,
}

impl Layer {
    fn new(name: &str, stuck_after: Duration) -> Self {
        Self {
            name: name.to_owned(),
            deck: Deck::new(stuck_after),
            paused: false,
            duck: None,
            ducked: Ramp::constant(1.0),
            release: 0,
            holding: 0,
            ducking: false,
            rendered: false,
            scratch: Vec::new(),
            broken: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether rendering it now would play anything.
    fn is_audible(&self) -> bool {
        !self.paused && self.deck.is_audible()
    }

    /// Renders `frames` into the layer's own buffer, dropping a block that is not finite: a NaN
    /// from one layer would otherwise silence every other one, and stay in the effects' state.
    fn render(&mut self, frames: usize) {
        let samples = frames * CHANNELS;
        if self.scratch.len() < samples {
            self.scratch.resize(samples, 0.0);
        }
        let out = &mut self.scratch[..samples];
        self.deck.render(out);
        if out.iter().all(|sample| sample.is_finite()) {
            self.broken = false;
            return;
        }
        out.fill(0.0);
        self.deck.reset_effects();
        if !self.broken {
            self.broken = true;
            tracing::warn!(layer = %self.name, "dropped audio that was not finite and reset the effects");
        }
    }

    /// Notes whether its clips were heard in this block, for its ducking.
    fn update_ducking(&mut self, frames: usize) {
        let Some(duck) = &self.duck else {
            self.ducking = false;
            return;
        };
        let sounding = self.rendered && self.deck.is_sounding();
        self.holding = if sounding {
            duck.hold
        } else {
            self.holding.saturating_sub(frames)
        };
        self.ducking = sounding || self.holding > 0;
    }
}

#[derive(Debug)]
pub struct Mixer {
    /// The music first.
    layers: Vec<Layer>,
    /// Layers removed while they still sounded, until they have faded out.
    retiring: Vec<Layer>,
    /// Events of retired layers, until they are drained.
    retired_events: Vec<DeckEvent>,
    /// Keeps the sum within full scale.
    master: Effects,
    /// Whether the last render had anything to play; the limiter's look-ahead still holds what
    /// came before a silence.
    was_audible: bool,
    /// What each layer is asked to duck to: the gain, how fast, and how fast back.
    asked: Vec<Option<(f32, usize, usize)>>,
    /// After waiting this long for media, a track is reported stuck.
    stuck_after: Duration,
}

impl Mixer {
    pub fn new(stuck_after: Duration) -> Self {
        Self {
            layers: vec![Layer::new(MAIN, stuck_after)],
            retiring: Vec::new(),
            retired_events: Vec::new(),
            master: Effects::limiter(),
            was_audible: false,
            asked: Vec::new(),
            stuck_after,
        }
    }

    /// The music's layer, which the queue plays into.
    pub fn main(&self) -> &Layer {
        &self.layers[0]
    }

    pub fn main_mut(&mut self) -> &mut Layer {
        &mut self.layers[0]
    }

    /// Every layer, the music first.
    pub fn layers(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter()
    }

    pub fn layer(&self, name: &str) -> Option<&Layer> {
        self.layers.iter().find(|layer| layer.name == name)
    }

    pub fn layer_mut(&mut self, name: &str) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|layer| layer.name == name)
    }

    /// The layer called `name`, added after the others if there is none yet.
    pub fn layer_or_add(&mut self, name: &str) -> &mut Layer {
        let index = match self.layers.iter().position(|layer| layer.name == name) {
            Some(index) => index,
            None => {
                self.layers.push(Layer::new(name, self.stuck_after));
                self.layers.len() - 1
            }
        };
        &mut self.layers[index]
    }

    /// Removes a layer other than the music's, ending everything it plays (reported as
    /// stopped); what it played fades out. Returns whether there was one.
    pub fn remove_layer(&mut self, name: &str) -> bool {
        let Some(index) = self
            .layers
            .iter()
            .position(|layer| layer.name == name && layer.name != MAIN)
        else {
            return false;
        };
        let mut layer = self.layers.remove(index);
        layer.deck.end_all(EndReason::Stopped, None);
        layer.duck = None;
        layer.paused = false;
        self.retiring.push(layer);
        true
    }

    /// Whether rendering would play anything: some layer that is not paused has a track.
    pub fn is_audible(&self) -> bool {
        self.layers
            .iter()
            .chain(&self.retiring)
            .any(Layer::is_audible)
    }

    /// The players of every track in every layer.
    pub fn players(&self) -> impl Iterator<Item = &Player> {
        self.layers
            .iter()
            .chain(&self.retiring)
            .flat_map(|layer| layer.deck.players())
    }

    /// Every layer's events, in the order of the layers.
    pub fn drain_events(&mut self) -> Vec<DeckEvent> {
        let mut events = std::mem::take(&mut self.retired_events);
        for layer in self.layers.iter_mut().chain(&mut self.retiring) {
            events.extend(layer.deck.drain_events());
        }
        events
    }

    /// Fills `out` (interleaved stereo) with the sum of the layers that are playing, ducked as
    /// their layers ask, within full scale. Never blocks.
    pub fn render(&mut self, out: &mut [f32]) {
        if !self.is_audible() {
            out.fill(0.0);
            self.was_audible = false;
            return;
        }
        if !self.was_audible {
            // what the limiter still holds is from before the silence
            self.master.reset();
            self.was_audible = true;
        }
        let frames = out.len() / CHANNELS;
        for layer in self.layers.iter_mut().chain(&mut self.retiring) {
            layer.rendered = layer.is_audible();
            if layer.rendered {
                layer.render(frames);
            }
            layer.update_ducking(frames);
        }
        self.duck();
        out.fill(0.0);
        for layer in self.layers.iter_mut().chain(&mut self.retiring) {
            if layer.rendered {
                layer.ducked.apply_add(&layer.scratch[..out.len()], out);
            }
        }
        self.master.process(out);

        let retired = &mut self.retired_events;
        self.retiring.retain_mut(|layer| {
            let sounds = layer.deck.is_audible();
            if !sounds {
                retired.extend(layer.deck.drain_events());
            }
            sounds
        });
    }

    /// Moves each layer's ducking gain towards the lowest one that a layer ducking it asks for,
    /// or back to none.
    fn duck(&mut self) {
        self.asked.clear();
        for layer in &self.layers {
            let mut asked: Option<(f32, usize, usize)> = None;
            for ducker in self
                .layers
                .iter()
                .filter(|d| d.ducking && d.name != layer.name)
            {
                let Some(duck) = &ducker.duck else { continue };
                if duck.layers.contains(&layer.name)
                    && asked.is_none_or(|(gain, ..)| duck.gain < gain)
                {
                    asked = Some((duck.gain, duck.attack, duck.release));
                }
            }
            self.asked.push(asked);
        }
        for (layer, asked) in self.layers.iter_mut().zip(&self.asked) {
            let (target, frames) = match *asked {
                Some((gain, attack, release)) => {
                    layer.release = release;
                    (gain, attack)
                }
                None => (1.0, layer.release),
            };
            if !layer.rendered {
                // nothing to fade while it is not heard
                layer.ducked = Ramp::constant(target);
            } else if layer.ducked.target() != target {
                layer.ducked.move_to(target, frames, Curve::Linear);
            }
        }
    }
}
