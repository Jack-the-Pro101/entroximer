//! A compiled graph at work: the playback rate, then the node network, over interleaved stereo.

use fundsp::MAX_BUFFER_SIZE;
use fundsp::audiounit::AudioUnit;
use fundsp::buffer::BufferVec;
use fundsp::prelude::limiter_stereo;

use super::CHANNELS;
use super::rate::RateStage;

/// What a [`GraphSpec`](super::GraphSpec) compiles to, from
/// [`NodeRegistry::compile`](super::NodeRegistry::compile).
pub struct Effects {
    /// `None` at rate 1.
    rate: Option<RateStage>,
    /// `None` for a graph that passes its input through unchanged.
    network: Option<Network>,
}

impl Effects {
    pub(super) fn new(rate: Option<RateStage>, network: Option<Box<dyn AudioUnit>>) -> Self {
        Self {
            rate,
            network: network.map(Network::new),
        }
    }

    /// Keeps the peaks of a mix within full scale, as the `limiter` node does at its defaults;
    /// delays it by its 5 ms of look-ahead.
    pub fn limiter() -> Self {
        let mut unit: Box<dyn AudioUnit> = Box::new(limiter_stereo(0.005, 0.25));
        unit.set_sample_rate(48_000.0);
        unit.reset();
        Self::new(None, Some(unit))
    }

    /// Processes interleaved stereo in place; for effects without a rate, which needs more
    /// input than it outputs.
    pub fn process(&mut self, audio: &mut [f32]) {
        debug_assert!(self.rate.is_none(), "effects with a rate need a source");
        if let Some(network) = &mut self.network {
            network.process(audio);
        }
    }

    /// Fills `out` (interleaved stereo) with processed audio, taking the input from `source`.
    ///
    /// `source` must fill every buffer it is given completely, with silence where it has
    /// nothing. Away from rate 1 it is asked for more or fewer frames than `out` holds, in
    /// chunks of its own size.
    pub fn render(&mut self, out: &mut [f32], mut source: impl FnMut(&mut [f32])) {
        match &mut self.rate {
            Some(rate) => rate.render(out, &mut source),
            None => source(out),
        }
        if let Some(network) = &mut self.network {
            network.process(out);
        }
    }

    /// How many source frames one output frame takes: 1 unless the graph sets a rate.
    pub fn rate(&self) -> f32 {
        self.rate.as_ref().map_or(1.0, RateStage::rate)
    }

    /// Clears internal state such as reverb tails and resampled audio, e.g. after a seek.
    pub fn reset(&mut self) {
        if let Some(rate) = &mut self.rate {
            rate.reset();
        }
        if let Some(network) = &mut self.network {
            network.unit.reset();
        }
    }
}

impl std::fmt::Debug for Effects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effects")
            .field("rate", &self.rate())
            .field("network", &self.network.is_some())
            .finish()
    }
}

/// Runs a stereo unit over interleaved audio in fundsp-sized blocks.
struct Network {
    unit: Box<dyn AudioUnit>,
    input: BufferVec,
    output: BufferVec,
}

impl Network {
    /// # Panics
    /// If `unit` is not stereo in and stereo out; compiled graphs always are.
    fn new(unit: Box<dyn AudioUnit>) -> Self {
        assert_eq!(
            (unit.inputs(), unit.outputs()),
            (CHANNELS, CHANNELS),
            "graph units must be stereo in, stereo out"
        );
        Self {
            unit,
            input: BufferVec::new(CHANNELS),
            output: BufferVec::new(CHANNELS),
        }
    }

    /// Processes interleaved stereo in place.
    fn process(&mut self, interleaved: &mut [f32]) {
        for block in interleaved.chunks_mut(MAX_BUFFER_SIZE * CHANNELS) {
            let (frames, _) = block.as_chunks_mut::<CHANNELS>();
            for (i, frame) in frames.iter().enumerate() {
                for (channel, &sample) in frame.iter().enumerate() {
                    self.input.set_f32(channel, i, sample);
                }
            }
            self.unit.process(
                frames.len(),
                &self.input.buffer_ref(),
                &mut self.output.buffer_mut(),
            );
            for (i, frame) in frames.iter_mut().enumerate() {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = self.output.at_f32(channel, i);
                }
            }
        }
    }
}
