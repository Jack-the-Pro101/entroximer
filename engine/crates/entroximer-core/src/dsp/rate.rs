//! Playback rate: plays a stream faster and higher, or slower and lower, like a turntable at
//! another speed.
//!
//! Done by resampling: for every output frame, `rate` frames of the source are consumed. The
//! stage pulls exactly as much source audio as the resampler needs for each chunk, so the
//! source's position advances `rate` times as fast as the output.

use std::collections::VecDeque;
use std::ops::RangeInclusive;

use rubato::{
    Resampler as _, ResamplerConstructionError, SincFixedOut, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use super::CHANNELS;

/// Rates graphs may ask for.
pub const RATE_RANGE: RangeInclusive<f32> = 0.5..=2.0;

/// Output frames per resampler call: small enough to add little latency and memory.
const CHUNK_FRAMES: usize = 256;

pub(super) struct RateStage {
    rate: f32,
    resampler: SincFixedOut<f32>,
    /// Source audio for one chunk, interleaved as the source delivers it.
    interleaved: Vec<f32>,
    /// The same, one buffer per channel, as the resampler takes it.
    planar_in: [Vec<f32>; CHANNELS],
    planar_out: Vec<Vec<f32>>,
    /// Resampled interleaved audio not handed out yet.
    ready: VecDeque<f32>,
}

impl RateStage {
    pub(super) fn new(rate: f32) -> Result<Self, ResamplerConstructionError> {
        let parameters = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            oversampling_factor: 128,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        // the resampler's ratio is output frames per input frame
        let resampler = SincFixedOut::new(
            1.0 / f64::from(rate),
            1.0,
            parameters,
            CHUNK_FRAMES,
            CHANNELS,
        )?;
        let planar_out = resampler.output_buffer_allocate(true);
        Ok(Self {
            rate,
            resampler,
            interleaved: Vec::new(),
            planar_in: Default::default(),
            planar_out,
            ready: VecDeque::with_capacity(2 * CHUNK_FRAMES * CHANNELS),
        })
    }

    pub(super) fn rate(&self) -> f32 {
        self.rate
    }

    /// Fills `out` (interleaved stereo) with the source played at the stage's rate. `source`
    /// must fill the buffers it is given completely.
    pub(super) fn render(&mut self, out: &mut [f32], source: &mut impl FnMut(&mut [f32])) {
        while self.ready.len() < out.len() {
            self.resample_chunk(source);
        }
        let samples = out.len();
        for (sample, ready) in out.iter_mut().zip(self.ready.drain(..samples)) {
            *sample = ready;
        }
    }

    /// Forgets buffered audio, e.g. after a seek.
    pub(super) fn reset(&mut self) {
        self.resampler.reset();
        self.ready.clear();
    }

    fn resample_chunk(&mut self, source: &mut impl FnMut(&mut [f32])) {
        let needed = self.resampler.input_frames_next();
        self.interleaved.resize(needed * CHANNELS, 0.0);
        source(&mut self.interleaved);
        for (channel, planar) in self.planar_in.iter_mut().enumerate() {
            planar.clear();
            planar.extend(self.interleaved.iter().skip(channel).step_by(CHANNELS));
        }
        let (_, produced) = self
            .resampler
            .process_into_buffer(&self.planar_in, &mut self.planar_out, None)
            .expect("buffers are sized by the resampler");
        let [left, right] = [&self.planar_out[0], &self.planar_out[1]];
        for (&l, &r) in left[..produced].iter().zip(&right[..produced]) {
            self.ready.extend([l, r]);
        }
    }
}

impl std::fmt::Debug for RateStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateStage")
            .field("rate", &self.rate)
            .field("ready_frames", &(self.ready.len() / CHANNELS))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::*;

    /// A 1 kHz stereo sine source, counting the frames it delivered.
    fn sine_source(consumed: &mut usize) -> impl FnMut(&mut [f32]) + '_ {
        move |buffer: &mut [f32]| {
            for frame in buffer.as_chunks_mut::<CHANNELS>().0 {
                let value = (TAU * 1000.0 * *consumed as f32 / 48_000.0).sin();
                frame.fill(value);
                *consumed += 1;
            }
        }
    }

    fn zero_crossings(interleaved: &[f32]) -> usize {
        let left: Vec<f32> = interleaved.iter().step_by(CHANNELS).copied().collect();
        left.windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count()
    }

    #[test]
    fn faster_rates_consume_more_and_sound_higher() {
        for rate in [0.8, 1.25, 2.0] {
            let mut stage = RateStage::new(rate).unwrap();
            let mut consumed = 0;
            let mut out = vec![0.0; 48_000 * CHANNELS];
            {
                let mut source = sine_source(&mut consumed);
                for block in out.chunks_mut(960 * CHANNELS) {
                    stage.render(block, &mut source);
                }
            }
            // a second of output took `rate` seconds of source, give or take one chunk
            let expected = 48_000.0 * rate;
            assert!(
                (consumed as f32 - expected).abs() < 2.0 * CHUNK_FRAMES as f32 * rate,
                "rate {rate}: consumed {consumed} frames"
            );
            let hz = zero_crossings(&out[4800 * CHANNELS..]) as f32 / 0.9;
            assert!(
                (hz - 1000.0 * rate).abs() < 15.0,
                "rate {rate}: heard {hz} Hz"
            );
        }
    }
}
