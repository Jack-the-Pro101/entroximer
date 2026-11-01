//! Runs a stereo unit over interleaved audio in fundsp-sized blocks.

use fundsp::MAX_BUFFER_SIZE;
use fundsp::audiounit::AudioUnit;
use fundsp::buffer::BufferVec;

use super::CHANNELS;

pub struct GraphRunner {
    unit: Box<dyn AudioUnit>,
    input: BufferVec,
    output: BufferVec,
}

impl GraphRunner {
    /// # Panics
    /// If `unit` is not stereo in and stereo out; compiled graphs always are.
    pub fn new(unit: Box<dyn AudioUnit>) -> Self {
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
    pub fn process(&mut self, interleaved: &mut [f32]) {
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

    /// Clears internal state such as reverb tails, e.g. after a seek.
    pub fn reset(&mut self) {
        self.unit.reset();
    }
}

impl std::fmt::Debug for GraphRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphRunner").finish_non_exhaustive()
    }
}
