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

    /// As [`read`](Self::read), interpolating cubically: smoother for delays that keep moving.
    /// `delay` is at least 1.
    fn read_cubic(&self, delay: f32) -> f32 {
        let len = self.buffer.len();
        let delay = delay.clamp(1.0, (len - 3) as f32);
        let whole = delay as usize;
        let newest = self.write + len - 1;
        let at = |back: usize| self.buffer[(newest - back) % len];
        let (x0, x1, x2, x3) = (at(whole - 1), at(whole), at(whole + 1), at(whole + 2));
        // Catmull-Rom between `x1` and `x2`
        let t = delay.fract();
        let c1 = 0.5 * (x2 - x0);
        let c2 = x0 - 2.5 * x1 + 2.0 * x2 - 0.5 * x3;
        let c3 = 0.5 * (x3 - x0) + 1.5 * (x1 - x2);
        ((c3 * t + c2) * t + c1) * t + x1
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
    /// `semitones` is how far the pitch swings either way, whatever the frequency: slower
    /// vibratos swing the delay further.
    pub(super) fn new(frequency: f32, semitones: f32) -> Self {
        // the delay `swing * (1 + sin(2πft)) / 2` changes the speed by up to `swing * π * f`
        let deviation = (semitones / 12.0).exp2() - 1.0;
        let mut vibrato = Self {
            lfo: Lfo::new(frequency),
            swing: deviation / (PI * frequency),
            sample_rate: DEFAULT_SAMPLE_RATE,
            lines: [DelayLine::new(0), DelayLine::new(0)],
        };
        vibrato.allocate();
        vibrato
    }

    fn allocate(&mut self) {
        let max_delay = (self.swing * self.sample_rate).ceil() as usize + 1;
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
    /// Makes up for the power the crossfade loses: what the two heads read, 25 ms apart, is
    /// mostly unrelated in music, so their powers add, and sin⁴ + cos⁴ averages 3/4.
    const OVERLAP_MAKEUP: f32 = 1.154_700_5;

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
        // sin² and cos² add up to one (before the makeup), and each head is silent where it jumps
        let first_gain = (PI * self.phase).sin().powi(2) * Self::OVERLAP_MAKEUP;
        let second_gain = Self::OVERLAP_MAKEUP - first_gain;

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

/// Evens out the level: whatever rises above the threshold is lowered by `ratio`, with a soft
/// knee. Both channels are lowered alike, so the stereo image stays.
#[derive(Clone, Debug)]
pub(super) struct Compressor {
    threshold_db: f32,
    /// The share of each dB above the threshold that is taken off: `1 / ratio - 1`.
    slope: f32,
    makeup: f32,
    /// Seconds to reach about two thirds of a larger and a smaller reduction.
    attack: f32,
    release: f32,
    /// Per-sample smoothing towards a larger and a smaller reduction.
    attack_coefficient: f32,
    release_coefficient: f32,
    /// The current gain reduction in dB, at most 0.
    reduction_db: f32,
}

impl Compressor {
    /// Width of the soft knee around the threshold, in dB.
    const KNEE_DB: f32 = 6.0;

    pub(super) fn new(
        threshold_db: f32,
        ratio: f32,
        attack: f32,
        release: f32,
        makeup_db: f32,
    ) -> Self {
        let mut compressor = Self {
            threshold_db,
            slope: 1.0 / ratio - 1.0,
            makeup: db_to_gain(makeup_db),
            attack,
            release,
            attack_coefficient: 0.0,
            release_coefficient: 0.0,
            reduction_db: 0.0,
        };
        compressor.set_coefficients(DEFAULT_SAMPLE_RATE);
        compressor
    }

    fn set_coefficients(&mut self, sample_rate: f32) {
        self.attack_coefficient = (-1.0 / (self.attack * sample_rate)).exp();
        self.release_coefficient = (-1.0 / (self.release * sample_rate)).exp();
    }

    /// The reduction, in dB, that a peak of `level_db` asks for.
    fn reduction(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let half_knee = Self::KNEE_DB / 2.0;
        if over <= -half_knee {
            0.0
        } else if over >= half_knee {
            self.slope * over
        } else {
            self.slope * (over + half_knee).powi(2) / (2.0 * Self::KNEE_DB)
        }
    }
}

impl AudioNode for Compressor {
    const ID: u64 = 0x6e78_0007;
    type Inputs = U2;
    type Outputs = U2;

    fn reset(&mut self) {
        self.reduction_db = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f64) {
        self.set_coefficients(sample_rate as f32);
    }

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        let peak = input[0].abs().max(input[1].abs()).max(1e-6);
        let target = self.reduction(20.0 * peak.log10());
        let coefficient = if target < self.reduction_db {
            self.attack_coefficient
        } else {
            self.release_coefficient
        };
        self.reduction_db = target + coefficient * (self.reduction_db - target);
        let gain = db_to_gain(self.reduction_db) * self.makeup;
        Frame::from([input[0] * gain, input[1] * gain])
    }
}

/// The shadow a head casts on one ear (Brown and Duda's spherical head model): low frequencies
/// bend around it, high ones, from about 1.2 kHz up, are scaled by `alpha`.
#[derive(Clone, Debug, Default)]
struct HeadShadow {
    previous_input: f32,
    previous_output: f32,
}

impl HeadShadow {
    /// Twice the speed of sound over the radius of a head, in radians per second.
    const CORNER: f32 = 2.0 * 343.0 / 0.0875;

    /// `bilinear` is twice the sample rate.
    fn process(&mut self, input: f32, alpha: f32, bilinear: f32) -> f32 {
        // (CORNER + alpha s) / (CORNER + s), through the bilinear transform
        let norm = 1.0 / (Self::CORNER + bilinear);
        let b0 = (Self::CORNER + alpha * bilinear) * norm;
        let b1 = (Self::CORNER - alpha * bilinear) * norm;
        let a1 = (Self::CORNER - bilinear) * norm;
        let output = b0 * input + b1 * self.previous_input - a1 * self.previous_output;
        self.previous_input = input;
        self.previous_output = output;
        output
    }
}

/// Circles the sound around the listener's head, "8D audio". The middle of the stereo image
/// becomes one source that orbits, heard through a simple model of a head: the far ear hears it
/// later, quieter and duller, and from behind both ears hear it duller. What differs between the
/// channels stays put, scaled by `width`.
#[derive(Clone, Debug)]
pub(super) struct Orbit {
    frequency: f32,
    /// Position on the circle, in `0..1`: 0 in front, a quarter to the right.
    phase: f32,
    step: f32,
    /// How far towards each side it goes, in `0..=1`.
    depth: f32,
    width: f32,
    sample_rate: f32,
    line: DelayLine,
    shadows: [HeadShadow; 2],
    /// What differs between the channels, a sample late as the orbiting middle is.
    difference: f32,
}

impl Orbit {
    /// How much later the far ear hears a sound from the side: about 0.65 ms for a human head.
    const MAX_INTERAURAL_DELAY: f32 = 0.00065;
    /// What the far ear keeps of the high frequencies of a sound from the side.
    const FAR_EAR: f32 = 0.15;
    /// What both ears keep of the high frequencies of a sound from behind.
    const BEHIND: f32 = 0.5;

    pub(super) fn new(frequency: f32, depth: f32, width: f32) -> Self {
        let mut orbit = Self {
            frequency,
            phase: 0.0,
            step: 0.0,
            depth,
            width,
            sample_rate: DEFAULT_SAMPLE_RATE,
            line: DelayLine::new(0),
            shadows: Default::default(),
            difference: 0.0,
        };
        orbit.allocate();
        orbit
    }

    fn allocate(&mut self) {
        self.step = self.frequency / self.sample_rate;
        // one more sample for the cubic interpolation
        let max_delay = (Self::MAX_INTERAURAL_DELAY * self.sample_rate).ceil() as usize + 3;
        self.line = DelayLine::new(max_delay);
    }
}

impl AudioNode for Orbit {
    const ID: u64 = 0x6e78_0008;
    type Inputs = U2;
    type Outputs = U2;

    fn reset(&mut self) {
        self.phase = 0.0;
        self.line.clear();
        self.shadows = Default::default();
        self.difference = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.allocate();
    }

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        // -1 fully left, 1 fully right; -1 behind, 1 in front
        let (side, front) = (TAU * self.phase).sin_cos();
        self.phase = (self.phase + self.step).fract();
        let side = self.depth * side;
        let behind = self.depth * (-front).max(0.0);

        let middle = 0.5 * (input[0] + input[1]);
        let difference = std::mem::replace(
            &mut self.difference,
            0.5 * (input[0] - input[1]) * self.width,
        );
        self.line.push(middle);

        // equal power, so the loudness stays even
        let angle = (side + 1.0) * FRAC_PI_4;
        let gains = [angle.cos() * SQRT_2, angle.sin() * SQRT_2];
        let lag = Self::MAX_INTERAURAL_DELAY * self.sample_rate * side;
        // one sample for every ear, so that the cubic interpolation has a newer sample
        let delays = [1.0 + lag.max(0.0), 1.0 + (-lag).max(0.0)];
        let dull = 1.0 - (1.0 - Self::BEHIND) * behind;
        let alphas = [
            (1.0 - (1.0 - Self::FAR_EAR) * side.max(0.0)) * dull,
            (1.0 - (1.0 - Self::FAR_EAR) * (-side).max(0.0)) * dull,
        ];
        let bilinear = 2.0 * self.sample_rate;
        let mut output = Frame::default();
        for ear in 0..2 {
            let heard = self.line.read_cubic(delays[ear]);
            output[ear] = self.shadows[ear].process(heard, alphas[ear], bilinear) * gains[ear];
        }
        output[0] += difference;
        output[1] -= difference;
        output
    }
}

/// The surface noise of a worn record: a soft hiss, and crackles at random moments and places
/// between the channels.
#[derive(Clone, Debug)]
pub(super) struct Vinyl {
    /// Peak amplitude of the hiss's white noise, and of the loudest crackles.
    hiss: f32,
    crackle: f32,
    /// Crackles per second.
    density: f32,
    sample_rate: f32,
    /// Chance per sample that a crackle starts.
    chance: f32,
    /// How much of a crackle's envelope is left after each sample.
    decay: f32,
    /// What smooths the hiss: the share of the new sample in a one-pole low-pass.
    smoothing: f32,
    noise: Noise,
    hissing: [f32; 2],
    /// The envelope of the current crackle in each channel.
    crackling: [f32; 2],
}

impl Vinyl {
    /// How long a crackle lasts, to about a third.
    const CRACKLE_SECONDS: f32 = 0.0004;
    /// Where the hiss starts rolling off, in Hz.
    const HISS_CUTOFF: f32 = 4000.0;

    /// `amount` in `0..=1` scales both the hiss and the crackles.
    pub(super) fn new(amount: f32, density: f32) -> Self {
        let mut vinyl = Self {
            hiss: 0.02 * amount,
            crackle: 0.3 * amount,
            density,
            sample_rate: DEFAULT_SAMPLE_RATE,
            chance: 0.0,
            decay: 0.0,
            smoothing: 0.0,
            noise: Noise::new(),
            hissing: [0.0; 2],
            crackling: [0.0; 2],
        };
        vinyl.set_coefficients();
        vinyl
    }

    fn set_coefficients(&mut self) {
        self.chance = self.density / self.sample_rate;
        self.decay = (-1.0 / (Self::CRACKLE_SECONDS * self.sample_rate)).exp();
        self.smoothing = 1.0 - (-TAU * Self::HISS_CUTOFF / self.sample_rate).exp();
    }
}

impl AudioNode for Vinyl {
    const ID: u64 = 0x6e78_0009;
    type Inputs = U2;
    type Outputs = U2;

    fn reset(&mut self) {
        self.noise = Noise::new();
        self.hissing = [0.0; 2];
        self.crackling = [0.0; 2];
    }

    fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.set_coefficients();
    }

    fn tick(&mut self, input: &Frame<f32, U2>) -> Frame<f32, U2> {
        if self.noise.unit() < self.chance {
            // mostly faint ones, a few loud
            let loudness = self.crackle * self.noise.unit().powi(3);
            let place = self.noise.unit();
            self.crackling = [loudness * (1.0 - place).sqrt(), loudness * place.sqrt()];
        }
        let mut output = Frame::default();
        for channel in 0..2 {
            let white = self.noise.signed();
            self.hissing[channel] += (white * self.hiss - self.hissing[channel]) * self.smoothing;
            let crackle = self.noise.signed() * self.crackling[channel];
            self.crackling[channel] *= self.decay;
            output[channel] = input[channel] + self.hissing[channel] + crackle;
        }
        output
    }
}

/// White noise from a xorshift generator: cheap, and the same every time it starts over.
#[derive(Clone, Debug)]
struct Noise(u32);

impl Noise {
    fn new() -> Self {
        Self(0x9e37_79b9)
    }

    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform in `0..1`.
    fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Uniform in `-1..1`.
    fn signed(&mut self) -> f32 {
        2.0 * self.unit() - 1.0
    }
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
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

    /// Root mean square of one channel of `frames`, in dB.
    fn level_db(frames: &[[f32; 2]], channel: usize) -> f32 {
        let power =
            frames.iter().map(|f| f[channel] * f[channel]).sum::<f32>() / frames.len() as f32;
        10.0 * power.log10()
    }

    /// `seconds` of a mono sine through `node`.
    fn sine_through(
        node: &mut impl AudioNode<Inputs = U2, Outputs = U2>,
        hz: f32,
        amplitude: f32,
        seconds: f32,
    ) -> Vec<[f32; 2]> {
        (0..(seconds * 48_000.0) as usize)
            .map(|i| {
                let x = amplitude * (TAU * hz * i as f32 / 48_000.0).sin();
                run(node, [x, x])
            })
            .collect()
    }

    #[test]
    fn compressor_lowers_what_is_above_the_threshold_by_its_ratio() {
        // 12 dB over the threshold: 9 dB lower once settled
        let mut compressor = Compressor::new(-20.0, 4.0, 0.001, 0.05, 0.0);
        let out = sine_through(&mut compressor, 100.0, db_to_gain(-8.0), 1.0);
        let peak = out[24_000..].iter().fold(0.0f32, |m, f| m.max(f[0].abs()));
        let peak_db = 20.0 * peak.log10();
        assert!((peak_db + 17.0).abs() < 1.0, "peaks at {peak_db:.1} dBFS");

        // below the threshold, only the makeup gain applies
        let mut quiet = Compressor::new(-20.0, 4.0, 0.001, 0.05, 6.0);
        let [left, right] = run(&mut quiet, [0.01, -0.01]);
        assert!((left - 0.01 * db_to_gain(6.0)).abs() < 1e-5);
        assert_eq!(left, -right);
    }

    #[test]
    fn orbit_circles_the_sound_around_the_head() {
        // one round per second: in front at 1 s, to the right at 0.25 s, behind at 0.5 s
        let window = |out: &[[f32; 2]], at: f32| {
            let middle = (at * 48_000.0) as usize;
            out[middle - 480..middle + 480].to_vec()
        };
        let out = sine_through(&mut Orbit::new(1.0, 0.8, 0.0), 500.0, 0.5, 1.1);
        let front = window(&out, 1.0);
        assert!((level_db(&front, 0) - level_db(&front, 1)).abs() < 0.5);
        let right = window(&out, 0.25);
        let difference = level_db(&right, 1) - level_db(&right, 0);
        assert!(difference > 10.0, "the right ear {difference:.1} dB louder");
        // the far ear hears it later: 0.8 of 0.65 ms (25 samples), and its head's shadow delays
        // 500 Hz by another 4, as a real head's does
        let lag = (0..40)
            .max_by(|&a, &b| {
                let correlation = |lag: usize| {
                    right[..900]
                        .iter()
                        .zip(&right[lag..])
                        .map(|(r, l)| r[1] * l[0])
                        .sum::<f32>()
                };
                correlation(a).total_cmp(&correlation(b))
            })
            .unwrap();
        assert!((27..=31).contains(&lag), "the left ear lags {lag} samples");

        // from behind, high frequencies are duller in both ears
        let out = sine_through(&mut Orbit::new(1.0, 0.8, 0.0), 8000.0, 0.5, 1.1);
        let (front, behind) = (window(&out, 1.0), window(&out, 0.5));
        let dulled = level_db(&front, 0) - level_db(&behind, 0);
        assert!(dulled > 3.0, "{dulled:.1} dB duller behind");
    }

    #[test]
    fn orbit_without_depth_keeps_the_stereo_image() {
        let mut orbit = Orbit::new(0.5, 0.0, 1.0);
        let mut previous = [0.0; 2];
        for i in 0..1000 {
            let input = [(i as f32 * 0.37).sin(), (i as f32 * 0.11).cos() * 0.5];
            let [left, right] = run(&mut orbit, input);
            assert!((left - previous[0]).abs() < 1e-4 && (right - previous[1]).abs() < 1e-4);
            previous = input;
        }
    }

    #[test]
    fn vibrato_swings_the_pitch_by_its_depth_even_when_slow() {
        let out = sine_through(&mut Vibrato::new(0.5, 1.0), 1000.0, 0.5, 2.0);
        // the frequency of every tenth of a second, from its upward zero crossings, once the
        // delay line has filled
        let frequencies: Vec<f32> = out[4800..]
            .chunks(4800)
            .map(|window| {
                let crossings = window
                    .windows(2)
                    .filter(|w| w[0][0] < 0.0 && w[1][0] >= 0.0);
                crossings.count() as f32 * 10.0
            })
            .collect();
        let highest = frequencies.iter().copied().fold(0.0, f32::max);
        let lowest = frequencies.iter().copied().fold(f32::INFINITY, f32::min);
        // a semitone either way: 1059 and 944 Hz
        assert!((1040.0..=1070.0).contains(&highest), "up to {highest} Hz");
        assert!((930.0..=960.0).contains(&lowest), "down to {lowest} Hz");
    }

    #[test]
    fn vinyl_hisses_and_crackles_the_same_every_time() {
        let mut vinyl = Vinyl::new(1.0, 50.0);
        let listen = |vinyl: &mut Vinyl| -> Vec<[f32; 2]> {
            (0..48_000).map(|_| run(vinyl, [0.0, 0.0])).collect()
        };
        let noise = listen(&mut vinyl);
        let level = level_db(&noise, 0);
        assert!((-50.0..=-25.0).contains(&level), "{level:.1} dBFS");
        let peak = noise
            .iter()
            .fold(0.0f32, |m, f| m.max(f[0].abs().max(f[1].abs())));
        assert!((0.05..=0.35).contains(&peak), "crackles peak at {peak}");
        vinyl.reset();
        assert_eq!(listen(&mut vinyl), noise);
        assert!(
            listen(&mut Vinyl::new(0.0, 50.0))
                .iter()
                .all(|f| *f == [0.0, 0.0])
        );
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
