//! Effects that fundsp has no ready-made unit for, written as plain per-frame processors.
//!
//! Every one of them is stereo in, stereo out. Parameters are fixed once built: changing an
//! effect means compiling the graph anew.

use std::f32::consts::{FRAC_PI_4, PI, SQRT_2, TAU};

use fundsp::prelude32::{AudioNode, Frame, U2};

/// What processors assume until the network tells them its sample rate.
const DEFAULT_SAMPLE_RATE: f32 = 48_000.0;

/// A sine oscillator for modulating other parameters.
#[derive(Clone, Debug)]
struct Lfo {
    frequency: f32,
    /// Position in the cycle, in `0..1`.
    phase: f32,
    step: f32,
}

impl Lfo {
    fn new(frequency: f32) -> Self {
        let mut lfo = Self {
            frequency,
            phase: 0.0,
            step: 0.0,
        };
        lfo.set_sample_rate(DEFAULT_SAMPLE_RATE);
        lfo
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.step = self.frequency / sample_rate;
    }

    /// The next value, in `-1..=1`.
    fn next(&mut self) -> f32 {
        let value = (TAU * self.phase).sin();
        self.phase = (self.phase + self.step).fract();
        value
    }
}

/// A delay line read at fractional delays, interpolating linearly.
#[derive(Clone, Debug)]
struct DelayLine {
    buffer: Vec<f32>,
    /// Where the next sample goes.
    write: usize,
}

impl DelayLine {
    /// A line that can delay by up to `max_delay` samples.
    fn new(max_delay: usize) -> Self {
        Self {
            buffer: vec![0.0; max_delay + 2],
            write: 0,
        }
    }

    fn push(&mut self, sample: f32) {
        self.buffer[self.write] = sample;
        self.write = (self.write + 1) % self.buffer.len();
    }

    /// The signal `delay` samples before the newest one pushed.
    fn read(&self, delay: f32) -> f32 {
        let len = self.buffer.len();
        let delay = delay.clamp(0.0, (len - 2) as f32);
        let whole = delay as usize;
        // `whole` is at most `len - 2`, so none of this goes below zero
        let newest = self.write + len - 1;
        let later = self.buffer[(newest - whole) % len];
        let earlier = self.buffer[(newest - whole - 1) % len];
        later + (earlier - later) * delay.fract()
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }
}

/// Raises and lowers the volume periodically.
#[derive(Clone, Debug)]
pub(super) struct Tremolo {
    lfo: Lfo,
    depth: f32,
}

impl Tremolo {
    /// `depth` in `0..=1`: how far the volume dips at the bottom of each cycle.
    pub(super) fn new(frequency: f32, depth: f32) -> Self {
        Self {
            lfo: Lfo::new(frequency),
            depth,
        }
    }
}

impl AudioNode for Tremolo {
    const ID: u64 = 0x6e78_0001;
    type Inputs = U2;
    type Outputs = U2;

    fn reset(&mut self) {
        self.lfo.phase = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f64) {
        self.lfo.set_sample_rate(sample_rate as f32);
    }

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        // between 1 and 1 - depth
        let gain = 1.0 - self.depth * (0.5 - 0.5 * self.lfo.next());
        Frame::from([input[0] * gain, input[1] * gain])
    }
}

/// Moves the sound from side to side, the "8D" effect. Equal-power, so the loudness stays even;
/// in the middle both channels are unchanged.
#[derive(Clone, Debug)]
pub(super) struct AutoPan {
    lfo: Lfo,
    /// How far towards each side it moves, in `0..=1`.
    depth: f32,
}

impl AutoPan {
    pub(super) fn new(frequency: f32, depth: f32) -> Self {
        Self {
            lfo: Lfo::new(frequency),
            depth,
        }
    }
}

impl AudioNode for AutoPan {
    const ID: u64 = 0x6e78_0002;
    type Inputs = U2;
    type Outputs = U2;

    fn reset(&mut self) {
        self.lfo.phase = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f64) {
        self.lfo.set_sample_rate(sample_rate as f32);
    }

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        // -1 is fully left, 1 fully right
        let position = self.depth * self.lfo.next();
        let angle = (position + 1.0) * FRAC_PI_4;
        Frame::from([
            input[0] * angle.cos() * SQRT_2,
            input[1] * angle.sin() * SQRT_2,
        ])
    }
}

/// Each output channel as a weighted sum of both input channels: mono, swapped, wider or
/// narrower stereo, or the difference of the channels (which removes what both share).
#[derive(Clone, Debug)]
pub(super) struct ChannelMix {
    pub(super) left_to_left: f32,
    pub(super) left_to_right: f32,
    pub(super) right_to_left: f32,
    pub(super) right_to_right: f32,
}

impl AudioNode for ChannelMix {
    const ID: u64 = 0x6e78_0003;
    type Inputs = U2;
    type Outputs = U2;

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        let (left, right) = (input[0], input[1]);
        Frame::from([
            left * self.left_to_left + right * self.right_to_left,
            left * self.left_to_right + right * self.right_to_right,
        ])
    }
}

/// Saturates the signal through a hyperbolic tangent: warmth at low drive, distortion at high.
#[derive(Clone, Debug)]
pub(super) struct Drive {
    drive: f32,
    /// Scales the curve so that a full-scale input stays at full scale.
    normalize: f32,
    /// Share of the saturated signal, in `0..=1`.
    mix: f32,
}

impl Drive {
    /// `drive` is at least 1.
    pub(super) fn new(drive: f32, mix: f32) -> Self {
        Self {
            drive,
            normalize: 1.0 / drive.tanh(),
            mix,
        }
    }

    fn shape(&self, sample: f32) -> f32 {
        let saturated = (sample * self.drive).tanh() * self.normalize;
        sample + (saturated - sample) * self.mix
    }
}

impl AudioNode for Drive {
    const ID: u64 = 0x6e78_0004;
    type Inputs = U2;
    type Outputs = U2;

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        Frame::from([self.shape(input[0]), self.shape(input[1])])
    }
}

/// Wobbles the pitch periodically by reading the signal through a delay that swings back and
/// forth.
#[derive(Clone, Debug)]
pub(super) struct Vibrato {
    lfo: Lfo,
    /// Largest swing of the delay, in seconds.
    swing: f32,
    sample_rate: f32,
    lines: [DelayLine; 2],
}

impl Vibrato {
    /// The delay swings by up to this much at full depth: about a semitone and a half either way
    /// at 5 Hz.
    const MAX_SWING: f32 = 0.005;

    /// `depth` in `0..=1`.
    pub(super) fn new(frequency: f32, depth: f32) -> Self {
        let mut vibrato = Self {
            lfo: Lfo::new(frequency),
            swing: depth * Self::MAX_SWING,
            sample_rate: DEFAULT_SAMPLE_RATE,
            lines: [DelayLine::new(0), DelayLine::new(0)],
        };
        vibrato.allocate();
        vibrato
    }

    fn allocate(&mut self) {
        let max_delay = (Self::MAX_SWING * self.sample_rate).ceil() as usize + 1;
        self.lines = [DelayLine::new(max_delay), DelayLine::new(max_delay)];
    }
}

impl AudioNode for Vibrato {
    const ID: u64 = 0x6e78_0005;
    type Inputs = U2;
    type Outputs = U2;

    fn reset(&mut self) {
        self.lfo.phase = 0.0;
        self.lines.iter_mut().for_each(DelayLine::clear);
    }

    fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.lfo.set_sample_rate(self.sample_rate);
        self.allocate();
    }

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        let delay = self.swing * (0.5 + 0.5 * self.lfo.next()) * self.sample_rate;
        let mut output = Frame::default();
        for (channel, line) in self.lines.iter_mut().enumerate() {
            line.push(input[channel]);
            output[channel] = line.read(delay);
        }
        output
    }
}

/// Shifts the pitch without changing the speed.
///
/// Two read heads move through a short delay line at the pitch ratio's speed; whenever one has
/// to jump back to stay within the line, it is faded out and the other, half a window away,
/// carries the sound. Good for voices and effects; chords smear a little.
#[derive(Clone, Debug)]
pub(super) struct PitchShift {
    ratio: f32,
    sample_rate: f32,
    /// Length of the stretch the read heads move through, in samples.
    window: f32,
    /// The first head's position in the window, in `0..1`.
    phase: f32,
    lines: [DelayLine; 2],
}

impl PitchShift {
    const WINDOW_SECONDS: f32 = 0.05;

    pub(super) fn new(semitones: f32) -> Self {
        let mut shift = Self {
            ratio: (semitones / 12.0).exp2(),
            sample_rate: DEFAULT_SAMPLE_RATE,
            window: 0.0,
            phase: 0.0,
            lines: [DelayLine::new(0), DelayLine::new(0)],
        };
        shift.allocate();
        shift
    }

    fn allocate(&mut self) {
        self.window = Self::WINDOW_SECONDS * self.sample_rate;
        let max_delay = self.window.ceil() as usize + 1;
        self.lines = [DelayLine::new(max_delay), DelayLine::new(max_delay)];
    }
}

impl AudioNode for PitchShift {
    const ID: u64 = 0x6e78_0006;
    type Inputs = U2;
    type Outputs = U2;

    fn reset(&mut self) {
        self.phase = 0.0;
        self.lines.iter_mut().for_each(DelayLine::clear);
    }

    fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.allocate();
    }

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        let second_phase = (self.phase + 0.5).fract();
        let (first_delay, second_delay) = (self.phase * self.window, second_phase * self.window);
        // sin² and cos² add up to one, and each head is silent where it jumps
        let first_gain = (PI * self.phase).sin().powi(2);
        let second_gain = 1.0 - first_gain;

        let mut output = Frame::default();
        for (channel, line) in self.lines.iter_mut().enumerate() {
            line.push(input[channel]);
            output[channel] =
                line.read(first_delay) * first_gain + line.read(second_delay) * second_gain;
        }
        // the delay changes by `1 - ratio` per sample, so the heads read at `ratio` speed
        self.phase = (self.phase + (1.0 - self.ratio) / self.window).rem_euclid(1.0);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(node: &mut impl AudioNode<Inputs = U2, Outputs = U2>, input: [f32; 2]) -> [f32; 2] {
        let output = node.tick(&Frame::from(input));
        [output[0], output[1]]
    }

    #[test]
    fn delay_lines_interpolate_between_samples() {
        let mut line = DelayLine::new(4);
        for sample in [1.0, 2.0, 3.0] {
            line.push(sample);
        }
        assert_eq!(line.read(0.0), 3.0);
        assert_eq!(line.read(1.0), 2.0);
        assert_eq!(line.read(1.5), 1.5);
        assert_eq!(
            line.read(99.0),
            line.read(4.0),
            "delays past the end are clamped"
        );
    }

    #[test]
    fn channel_mix_routes_channels() {
        let mut swap = ChannelMix {
            left_to_left: 0.0,
            left_to_right: 1.0,
            right_to_left: 1.0,
            right_to_right: 0.0,
        };
        assert_eq!(run(&mut swap, [1.0, 2.0]), [2.0, 1.0]);
    }

    #[test]
    fn drive_keeps_full_scale_and_mixes() {
        let mut drive = Drive::new(10.0, 1.0);
        let [full, quiet] = run(&mut drive, [1.0, 0.01]);
        assert!((full - 1.0).abs() < 1e-6);
        assert!(quiet > 0.09, "quiet signals are boosted, got {quiet}");
        let mut dry = Drive::new(10.0, 0.0);
        assert_eq!(run(&mut dry, [0.3, -0.3]), [0.3, -0.3]);
    }

    #[test]
    fn tremolo_and_auto_pan_stay_within_unity_power() {
        let mut tremolo = Tremolo::new(5.0, 1.0);
        let mut pan = AutoPan::new(5.0, 1.0);
        for _ in 0..48_000 {
            let [left, _] = run(&mut tremolo, [1.0, 1.0]);
            assert!((0.0..=1.0).contains(&left));
            let [left, right] = run(&mut pan, [1.0, 1.0]);
            assert!((left * left + right * right - 2.0).abs() < 1e-4);
        }
    }

    /// Counts upward zero crossings of the left channel over a second of a 440 Hz sine.
    fn output_frequency(node: &mut impl AudioNode<Inputs = U2, Outputs = U2>) -> f32 {
        let mut crossings = 0;
        let mut previous = 0.0;
        for i in 0..48_000 {
            let x = (TAU * 440.0 * i as f32 / 48_000.0).sin();
            let [left, _] = run(node, [x, x]);
            if previous < 0.0 && left >= 0.0 {
                crossings += 1;
            }
            previous = left;
        }
        crossings as f32
    }

    #[test]
    fn pitch_shift_moves_the_frequency_by_its_ratio() {
        let octave_up = output_frequency(&mut PitchShift::new(12.0));
        assert!((octave_up - 880.0).abs() < 25.0, "{octave_up} Hz");
        let fifth_down = output_frequency(&mut PitchShift::new(-7.0));
        let expected = 440.0 * (-7.0f32 / 12.0).exp2();
        assert!((fifth_down - expected).abs() < 15.0, "{fifth_down} Hz");
    }
}
