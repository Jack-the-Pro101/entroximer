//! One track sounding in a deck: its volume and fades, looping, its fade-out once it is no longer
//! wanted, and noticing when it stalls.

use std::time::Duration;

use crate::deck::{Clip, DeckTrack, TrackId};
use crate::dsp::{CHANNELS, Curve, Ramp};
use crate::player::{PlaybackError, Status as PlayerStatus};
use crate::timeline::Ticks;

/// How long a voice cut off takes to fade out: inaudible as a fade, but no click.
pub(crate) const DECLICK_FRAMES: usize = 960;

/// Why a voice stopped short of filling its buffer, or that it did not.
#[derive(Debug)]
pub(crate) enum Status {
    Playing,
    /// Media is missing; the rest of the buffer is to be silence.
    Buffering,
    /// It reached the end of its media or its end time.
    Ended,
    Failed(PlaybackError),
}

#[derive(Debug)]
pub(crate) struct Voice {
    pub(crate) track: DeckTrack,
    /// Its own volume; fades move between silence and it.
    volume: f32,
    gain: Ramp,
    /// How long it fades out when it is cut off.
    fade_out: usize,
    /// Plays again from the start whenever it reaches its end.
    looping: bool,
    /// Whether its owner hears about it. A voice that was reported ended can still sound for a
    /// moment as a tail, fading out, without telling anyone.
    pub(crate) reported: bool,
    /// Whether its last render had audio, i.e. it did not wait for media.
    pub(crate) sounding: bool,
    stuck_reported: bool,
}

impl Voice {
    pub(crate) fn new(mut track: DeckTrack) -> Self {
        track.player.set_end_at(track.end_at);
        Self {
            track,
            volume: 1.0,
            gain: Ramp::constant(1.0),
            fade_out: DECLICK_FRAMES,
            looping: false,
            reported: true,
            sounding: false,
            stuck_reported: false,
        }
    }

    pub(crate) fn clip(clip: Clip) -> Self {
        let mut voice = Self {
            volume: clip.volume,
            gain: Ramp::constant(clip.volume),
            fade_out: clip.fade_out.max(DECLICK_FRAMES),
            looping: clip.looping,
            ..Self::new(clip.track)
        };
        if clip.fade_in > 0 {
            voice.fade_in(clip.fade_in, Curve::Linear);
        }
        voice
    }

    pub(crate) fn id(&self) -> TrackId {
        self.track.id
    }

    /// Renders into the start of `out` (interleaved stereo, overwritten), up to the track's end;
    /// returns how many frames it wrote.
    pub(crate) fn render(&mut self, out: &mut [f32]) -> (usize, Status) {
        let (frames, status) = self.fill(out);
        self.gain.apply(&mut out[..frames * CHANNELS]);
        (frames, status)
    }

    /// Adds the voice to `out`, rendering it into `scratch` (as long as `out`) first.
    pub(crate) fn render_add(&mut self, scratch: &mut [f32], out: &mut [f32]) -> Status {
        let (frames, status) = self.fill(scratch);
        let samples = frames * CHANNELS;
        self.gain
            .apply_add(&scratch[..samples], &mut out[..samples]);
        status
    }

    /// The track's audio, before the gain.
    fn fill(&mut self, out: &mut [f32]) -> (usize, Status) {
        let frames = out.len() / CHANNELS;
        let mut filled = 0;
        // a loop that ends without having played anything would loop forever
        let mut looped_at = None;
        let status = loop {
            if filled == frames {
                break Status::Playing;
            }
            let player = &mut self.track.player;
            let limit = match self.track.end_at {
                Some(end) => usize::try_from(end - player.position()).unwrap_or(0),
                None => usize::MAX,
            };
            let wanted = (frames - filled).min(limit);
            let ended = if wanted == 0 {
                true
            } else {
                match player.render(&mut out[filled * CHANNELS..(filled + wanted) * CHANNELS]) {
                    Ok(rendered) => {
                        filled += rendered.frames;
                        match rendered.status {
                            PlayerStatus::Playing => false,
                            PlayerStatus::Buffering => break Status::Buffering,
                            PlayerStatus::Ended => true,
                        }
                    }
                    Err(e) => break Status::Failed(e),
                }
            };
            if ended {
                if !self.looping || looped_at == Some(filled) {
                    break Status::Ended;
                }
                looped_at = Some(filled);
                if let Err(e) = self.track.player.seek(0) {
                    break Status::Failed(e);
                }
            }
        };
        self.sounding = filled > 0;
        (filled, status)
    }

    pub(crate) fn seek(&mut self, position: Ticks) -> Result<(), PlaybackError> {
        self.stuck_reported = false;
        self.track.player.seek(position)
    }

    /// Starts the voice silent, fading in to its volume over `frames`.
    pub(crate) fn fade_in(&mut self, frames: usize, curve: Curve) {
        self.gain = Ramp::constant(0.0);
        self.gain.move_to(self.volume, frames, curve);
    }

    /// Turns the voice into a tail: no longer reported, fading out over `frames`.
    pub(crate) fn fade_out(&mut self, frames: usize, curve: Curve) {
        self.reported = false;
        self.gain.move_to(0.0, frames, curve);
    }

    /// Turns the voice into a tail that fades out as it does when cut off, or over `frames`.
    pub(crate) fn cut(&mut self, frames: Option<usize>) {
        let frames = frames.unwrap_or(self.fade_out).max(DECLICK_FRAMES);
        self.fade_out(frames, Curve::Linear);
    }

    /// Whether a tail has faded out completely.
    pub(crate) fn is_faded_out(&self) -> bool {
        !self.reported && self.gain.is_settled() && self.gain.target() == 0.0
    }

    /// How long it has waited for media, once per stall that lasts `after` or longer.
    pub(crate) fn check_stuck(&mut self, after: Duration) -> Option<Duration> {
        match self
            .track
            .player
            .buffering_since()
            .map(|since| since.elapsed())
        {
            Some(waiting) if waiting >= after && !self.stuck_reported => {
                self.stuck_reported = true;
                Some(waiting)
            }
            Some(_) => None,
            None => {
                self.stuck_reported = false;
                None
            }
        }
    }
}
