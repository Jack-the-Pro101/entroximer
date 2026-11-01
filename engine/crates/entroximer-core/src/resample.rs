//! Streaming stereo resampling to 48 kHz.

use rubato::{FftFixedIn, Resampler as _, ResamplerConstructionError};

#[derive(Debug, thiserror::Error)]
#[error("cannot resample {from} Hz to {to} Hz")]
pub struct ResampleError {
    from: u32,
    to: u32,
    #[source]
    source: ResamplerConstructionError,
}

pub struct Resampler {
    inner: FftFixedIn<f32>,
    input: [Vec<f32>; 2],
    output: Vec<Vec<f32>>,
    /// Output frames still to drop so that output time lines up with input time.
    skip: usize,
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resampler")
            .field("buffered_frames", &self.input[0].len())
            .finish_non_exhaustive()
    }
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Result<Self, ResampleError> {
        let inner = FftFixedIn::new(from as usize, to as usize, 1024, 2, 2)
            .map_err(|source| ResampleError { from, to, source })?;
        let output = inner.output_buffer_allocate(true);
        let skip = inner.output_delay();
        Ok(Self {
            inner,
            input: [Vec::new(), Vec::new()],
            output,
            skip,
        })
    }

    /// Feeds interleaved stereo; appends whatever resampled interleaved stereo is ready.
    pub fn process(&mut self, interleaved: &[f32], out: &mut Vec<f32>) {
        let (frames, _) = interleaved.as_chunks::<2>();
        self.input[0].extend(frames.iter().map(|&[left, _]| left));
        self.input[1].extend(frames.iter().map(|&[_, right]| right));
        loop {
            let need = self.inner.input_frames_next();
            if self.input[0].len() < need {
                break;
            }
            let chunk = [&self.input[0][..need], &self.input[1][..need]];
            let (_, produced) = self
                .inner
                .process_into_buffer(&chunk, &mut self.output, None)
                .expect("buffers are sized by the resampler");
            let skip = self.skip.min(produced);
            self.skip -= skip;
            let [left, right] = [
                &self.output[0][skip..produced],
                &self.output[1][skip..produced],
            ];
            out.extend(left.iter().zip(right).flat_map(|(&l, &r)| [l, r]));
            self.input[0].drain(..need);
            self.input[1].drain(..need);
        }
    }

    pub fn reset(&mut self) {
        self.inner.reset();
        self.input[0].clear();
        self.input[1].clear();
        self.skip = self.inner.output_delay();
    }
}
