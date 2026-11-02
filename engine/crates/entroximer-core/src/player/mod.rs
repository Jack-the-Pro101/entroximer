//! Turns a producer into 48 kHz stereo PCM, with seeking.
//!
//! The player owns the track's [`Timeline`] and the producers filling it. Rendering never blocks:
//! when media is missing it reports [`Status::Buffering`] and asks the coverage policy how to get
//! it (wait for the download, or re-open the source closer to the position).

mod coverage;

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use symphonia::core::codecs::audio::AudioCodecParameters;
use tokio::runtime::Handle;

pub use coverage::{Coverage, CoverageConfig};

use crate::decode::{DecodeError, Decoder};
use crate::demux::DemuxTask;
use crate::dsp::CHANNELS;
use crate::error::{MediaError, display_chain};
use crate::producer::{Producer, ProducerCaps};
use crate::resample::{ResampleError, Resampler};
use crate::timeline::{
    Lookup, Packet, Pending, RangeState, RetentionConfig, TICKS_PER_SECOND, Ticks, Timeline,
    TimelineStats, ticks,
};

/// Output sample rate (Discord's).
pub const SAMPLE_RATE: u32 = 48_000;

/// Preroll used before the codec is known; at least as long as any codec's own preroll.
const DEFAULT_PREROLL: Ticks = TICKS_PER_SECOND / 10;

/// Consecutive undecodable packets replaced by silence before playback fails.
const MAX_DECODE_FAILURES: u32 = 8;

#[derive(Clone, Debug)]
pub struct PlayerConfig {
    pub retention: RetentionConfig,
    pub coverage: CoverageConfig,
    /// Producer failures in a row that are retried before playback fails.
    pub max_producer_retries: u32,
    /// Wait before the first retry; doubled for each further one.
    pub retry_backoff: Duration,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            retention: RetentionConfig::default(),
            coverage: CoverageConfig::default(),
            max_producer_retries: 4,
            retry_backoff: Duration::from_millis(500),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Playing,
    /// Media is missing; the part of the output that was not rendered should be silence.
    Buffering,
    /// Everything up to the end of the media has been rendered.
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// Frames written to the start of the output buffer.
    pub frames: usize,
    pub status: Status,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("the media could not be loaded")]
    Media(#[source] MediaError),
    #[error("the media could not be decoded")]
    Decode(#[source] DecodeError),
    #[error(transparent)]
    Resample(#[from] ResampleError),
    #[error("failed to read cached media")]
    Cache(#[source] io::Error),
}

/// Decoder and resampler, created once the first producer has reported the codec.
struct Pipeline {
    decoder: Decoder,
    resampler: Option<Resampler>,
    preroll: Ticks,
}

impl Pipeline {
    fn new(params: &AudioCodecParameters) -> Result<Self, PlaybackError> {
        let decoder = Decoder::new(params).map_err(PlaybackError::Decode)?;
        let resampler = match decoder.sample_rate() {
            SAMPLE_RATE => None,
            rate => Some(Resampler::new(rate, SAMPLE_RATE)?),
        };
        Ok(Self {
            decoder,
            resampler,
            preroll: Decoder::preroll(params),
        })
    }

    fn reset(&mut self) -> Result<(), PlaybackError> {
        self.decoder.reset().map_err(PlaybackError::Decode)?;
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }
        Ok(())
    }
}

pub struct Player {
    rt: Handle,
    source: Arc<dyn Producer>,
    caps: ProducerCaps,
    timeline: Arc<Timeline>,
    cfg: PlayerConfig,
    /// Live producers; dropping one cancels it.
    producers: Vec<DemuxTask>,
    pipeline: Option<Pipeline>,
    /// Media time of the next packet to decode.
    cursor: Ticks,
    /// Decoded 48 kHz stereo waiting to be rendered.
    pcm: VecDeque<f32>,
    /// Media time just past the last frame in `pcm`; unknown until a packet follows a reset.
    pcm_end: Option<Ticks>,
    /// Pending seek: audio before it is decoded (preroll) but dropped.
    seek_target: Option<Ticks>,
    decoded: Vec<f32>,
    resampled: Vec<f32>,
    producer_failures: u32,
    /// No producer is opened before this, after a failure.
    retry_after: Option<Instant>,
    decode_failures: u32,
    buffering_since: Option<Instant>,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("position", &self.position())
            .field("caps", &self.caps)
            .field("producers", &self.producers)
            .field("pipeline", &self.pipeline.as_ref().map(|p| &p.decoder))
            .field("timeline", &self.timeline)
            .finish_non_exhaustive()
    }
}

impl Player {
    /// Starts loading `source` at `start` (the beginning if `None`), using the runtime `rt`.
    pub fn new(
        rt: Handle,
        source: Arc<dyn Producer>,
        cfg: PlayerConfig,
        start: Option<Ticks>,
    ) -> Self {
        let caps = source.caps();
        let timeline = Arc::new(Timeline::new(cfg.retention.clone()));
        let mut player = Self {
            rt,
            source,
            caps,
            timeline,
            cfg,
            producers: Vec::new(),
            pipeline: None,
            cursor: 0,
            pcm: VecDeque::new(),
            pcm_end: None,
            seek_target: None,
            decoded: Vec::new(),
            resampled: Vec::new(),
            producer_failures: 0,
            retry_after: None,
            decode_failures: 0,
            buffering_since: None,
        };
        player.reposition(start.unwrap_or(0).max(0));
        let plan = coverage::plan(
            Pending::Gap,
            player.cursor,
            caps,
            None,
            &player.cfg.coverage,
        );
        player.apply(plan);
        player
    }

    /// Fills `out` (interleaved stereo) with as many frames as are ready. Never blocks.
    pub fn render(&mut self, out: &mut [f32]) -> Result<Rendered, PlaybackError> {
        let wanted = out.len() / CHANNELS;
        self.timeline.set_playhead(self.position());
        let status = self.fill(wanted)?;

        let frames = wanted.min(self.pcm.len() / CHANNELS);
        let samples = frames * CHANNELS;
        let (front, back) = self.pcm.as_slices();
        let from_front = samples.min(front.len());
        out[..from_front].copy_from_slice(&front[..from_front]);
        out[from_front..samples].copy_from_slice(&back[..samples - from_front]);
        self.pcm.drain(..samples);

        let status = match status {
            Status::Ended if !self.pcm.is_empty() => Status::Playing,
            status => status,
        };
        self.buffering_since = match status {
            Status::Buffering => Some(self.buffering_since.unwrap_or_else(Instant::now)),
            Status::Playing | Status::Ended => None,
        };
        Ok(Rendered { frames, status })
    }

    /// Jumps to `target`. Instant when it is buffered; otherwise rendering reports
    /// `Buffering` until the media arrives.
    pub fn seek(&mut self, target: Ticks) -> Result<(), PlaybackError> {
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.reset()?;
        }
        self.reposition(target.max(0));
        match self.timeline.pending_at(self.cursor) {
            Some(pending) => self.cover(pending, self.cursor),
            None => Ok(()),
        }
    }

    /// Media time of the next frame `render` will output.
    pub fn position(&self) -> Ticks {
        if let Some(target) = self.seek_target {
            return target;
        }
        match self.pcm_end {
            Some(end) => end - (self.pcm.len() / CHANNELS) as Ticks,
            None => self.cursor,
        }
    }

    pub fn caps(&self) -> ProducerCaps {
        self.caps
    }

    /// Buffered `(start, end)` intervals.
    pub fn buffered(&self) -> Vec<(Ticks, Ticks)> {
        self.timeline.buffered()
    }

    /// How much media is downloaded from the position on, without a gap.
    pub fn buffered_ahead(&self) -> Ticks {
        let position = self.position();
        self.timeline
            .range_at(position)
            .map_or(0, |(_, end, _)| end - position)
    }

    /// Length of the media, once a producer has reached its end.
    pub fn media_end(&self) -> Option<Ticks> {
        self.timeline.media_end()
    }

    pub fn cache_stats(&self) -> TimelineStats {
        self.timeline.stats()
    }

    /// When the current stretch of buffering began, if rendering is buffering.
    pub fn buffering_since(&self) -> Option<Instant> {
        self.buffering_since
    }

    /// Blocks until the timeline changes or `timeout` passes; for callers that render offline.
    pub fn wait_for_media(&self, timeout: Duration) {
        self.timeline.wait(timeout);
    }

    /// Clears decoded audio and points the cursor at `target`, minus preroll.
    fn reposition(&mut self, target: Ticks) {
        self.pcm.clear();
        self.pcm_end = None;
        self.seek_target = Some(target);
        let preroll = self
            .pipeline
            .as_ref()
            .map_or(DEFAULT_PREROLL, |p| p.preroll);
        // don't preroll into a gap before the buffered range holding the target
        let earliest = self
            .timeline
            .range_at(target)
            .map_or(0, |(start, ..)| start);
        self.cursor = (target - preroll).max(earliest);
        self.timeline.set_playhead(target);
    }

    fn fill(&mut self, wanted: usize) -> Result<Status, PlaybackError> {
        while self.pcm.len() < wanted * CHANNELS {
            match self
                .timeline
                .lookup(self.cursor)
                .map_err(PlaybackError::Cache)?
            {
                Lookup::Ready(packet) => {
                    self.cursor = packet.end();
                    self.producer_failures = 0;
                    self.decode(&packet)?;
                }
                Lookup::Pending(pending) => {
                    self.cover(pending, self.cursor)?;
                    return Ok(Status::Buffering);
                }
                Lookup::End => return Ok(Status::Ended),
            }
        }
        self.prefetch()?;
        Ok(Status::Playing)
    }

    fn decode(&mut self, packet: &Packet) -> Result<(), PlaybackError> {
        let pipeline = match self.pipeline {
            Some(ref mut pipeline) => pipeline,
            None => {
                let params = self
                    .timeline
                    .codec()
                    .expect("producers pin the codec before pushing packets");
                self.pipeline.insert(Pipeline::new(&params)?)
            }
        };

        self.decoded.clear();
        match pipeline.decoder.decode(packet, &mut self.decoded) {
            Ok(_) => self.decode_failures = 0,
            Err(e) if self.decode_failures < MAX_DECODE_FAILURES => {
                self.decode_failures += 1;
                tracing::warn!(
                    pts = packet.pts,
                    error = %display_chain(&e),
                    "replacing undecodable packet with silence"
                );
                // silence at the output rate keeps every later timestamp in place
                self.resampled.clear();
                self.resampled
                    .resize(packet.dur.max(0) as usize * CHANNELS, 0.0);
                self.append_pcm(packet.pts, Output::Resampled);
                return Ok(());
            }
            Err(e) => return Err(PlaybackError::Decode(e)),
        }

        let output = match &mut pipeline.resampler {
            Some(resampler) => {
                self.resampled.clear();
                resampler.process(&self.decoded, &mut self.resampled);
                Output::Resampled
            }
            None => Output::Decoded,
        };
        self.append_pcm(packet.pts, output);
        Ok(())
    }

    /// Moves freshly decoded audio into `pcm`, dropping what precedes a pending seek target.
    fn append_pcm(&mut self, packet_pts: Ticks, output: Output) {
        let samples = match output {
            Output::Decoded => &self.decoded,
            Output::Resampled => &self.resampled,
        };
        let start = *self.pcm_end.get_or_insert(packet_pts);
        let frames = samples.len() / CHANNELS;
        let skip = self.seek_target.map_or(0, |target| {
            usize::try_from(target - start).unwrap_or(0).min(frames)
        });
        self.pcm.extend(&samples[skip * CHANNELS..]);
        let end = start + frames as Ticks;
        self.pcm_end = Some(end);
        if self.seek_target.is_some_and(|target| end >= target) {
            self.seek_target = None;
        }
    }

    fn cover(&mut self, pending: Pending, t: Ticks) -> Result<(), PlaybackError> {
        self.producers.retain(|p| !p.is_finished());
        let rate = match pending {
            Pending::Frontier { producer, .. } => self.timeline.producer_rate(producer),
            Pending::Opening { .. } | Pending::Gap => None,
        };
        let plan = coverage::plan(pending, t, self.caps, rate, &self.cfg.coverage);
        let Coverage::Open { at, exclusive } = plan else {
            return Ok(());
        };

        if let Some(error) = self.timeline.take_failure() {
            self.producer_failures += 1;
            if self.producer_failures > self.cfg.max_producer_retries {
                return Err(PlaybackError::Media(error));
            }
            // network sources often refuse briefly; back off exponentially before retrying
            let backoff = self
                .cfg
                .retry_backoff
                .saturating_mul(2u32.saturating_pow(self.producer_failures - 1));
            tracing::warn!(
                attempt = self.producer_failures,
                retry_in = ?backoff,
                error = %display_chain(&error),
                "producer failed, retrying"
            );
            self.retry_after = Some(Instant::now() + backoff);
        }
        if self.retry_after.is_some_and(|retry| Instant::now() < retry) {
            return Ok(());
        }
        self.retry_after = None;

        tracing::debug!(
            ?pending,
            t,
            ?rate,
            ?at,
            exclusive,
            "opening producer to cover position"
        );
        self.apply(plan);
        Ok(())
    }

    fn apply(&mut self, plan: Coverage) {
        if let Coverage::Open { at, exclusive } = plan {
            if exclusive {
                self.producers.clear();
            }
            self.producers.push(DemuxTask::spawn(
                &self.rt,
                self.timeline.clone(),
                self.source.as_ref(),
                at,
            ));
        }
    }

    /// Starts filling the gap after an interrupted range before playback reaches it.
    fn prefetch(&mut self) -> Result<(), PlaybackError> {
        if !self.caps.time_seek {
            return Ok(());
        }
        let Some((_, end, RangeState::Truncated)) = self.timeline.range_at(self.cursor) else {
            return Ok(());
        };
        if end - self.cursor > ticks(self.cfg.coverage.prefetch) {
            return Ok(());
        }
        match self.timeline.pending_at(end) {
            Some(pending @ Pending::Gap) => self.cover(pending, end),
            _ => Ok(()),
        }
    }
}

/// Which scratch buffer holds the 48 kHz output of the last decode.
enum Output {
    Decoded,
    Resampled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_can_move_to_an_audio_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Player>();
    }
}
