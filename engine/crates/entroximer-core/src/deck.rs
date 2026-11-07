//! One continuous audio output: the playing track, an optional preloaded next track, effects and
//! volume.
//!
//! A deck never ends: with nothing to play it outputs silence. When the current track finishes,
//! the preloaded next track takes over within the same render call, so tracks follow each other
//! without a gap. Effects (including a playback rate) and volume belong to the deck, so they
//! carry across tracks, and effect tails (reverb, echo) ring out into whatever follows.
//!
//! With a crossfade, the next track starts that long before the current one ends, fading in as
//! the current one fades out. The switch is reported when the fade starts, in the same order as
//! without one (the current track ended, the next started); the rest of the old track plays on as
//! a *tail*, unreported. A track that is cut off (replaced, skipped, stopped) is reported ended
//! right away too, then fades out over a few milliseconds as a tail, so that it does not click.
//!
//! A deck also plays *clips*, sounds outside any queue (see [`Deck::place`]): one after another
//! in its current and next slots, or several at once over each other.
//!
//! Everything that happens is reported as [`DeckEvent`]s, drained by the owner.

use std::time::Duration;

use crate::dsp::{CHANNELS, Curve, Effects, Strip};
use crate::player::{PlaybackError, Player};
use crate::timeline::{TICKS_PER_SECOND, Ticks};
use crate::voice::{DECLICK_FRAMES, Status, Voice};

/// Identifies a track to the deck's owner; the deck only passes it back in events.
pub type TrackId = u64;

/// Tracks fading out at once; older ones are dropped.
const MAX_TAILS: usize = 3;
/// A crossfade shorter than this is not worth it: the tracks follow without a gap instead.
const MIN_CROSSFADE: Ticks = TICKS_PER_SECOND / 4;
/// How much of the next track must be downloaded for a crossfade to start.
const READY_AHEAD: Ticks = TICKS_PER_SECOND;

#[derive(Debug)]
pub struct DeckTrack {
    pub id: TrackId,
    pub player: Player,
    /// Where playback started, e.g. a queue item's start time; a crossfade covers at most half of
    /// what plays from there.
    pub start: Ticks,
    /// Stop when playback reaches this position.
    pub end_at: Option<Ticks>,
    /// The track's length as its source describes it, for a crossfade to start before the media
    /// has downloaded to its end.
    pub length: Option<Ticks>,
}

/// A sound for [`Deck::place`], with how it plays.
#[derive(Debug)]
pub struct Clip {
    pub track: DeckTrack,
    /// Linear gain of this clip alone.
    pub volume: f32,
    /// Frames it takes to fade in from silence; 0 starts at full volume.
    pub fade_in: usize,
    /// Frames it takes to fade out when cut off; a few milliseconds at least.
    pub fade_out: usize,
    /// Plays again from the start whenever it ends, until it is ended.
    pub looping: bool,
}

/// What a clip does to the clips a deck plays already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipMode {
    /// Waits its turn: plays next, after the current one.
    Queue,
    /// Plays over the others, at most this many at once; the oldest one ends to make room.
    Mix(usize),
    /// Ends every other one, and the one waiting.
    Replace,
}

/// Where [`Deck::place`] put a clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// It started (a `Started` event says so).
    Playing,
    /// It plays once the current one ends.
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// Reached the end of the media or its end time.
    Finished,
    /// Stopped by the owner.
    Stopped,
    /// Another track was started in its place.
    Replaced,
    /// Playback failed; a `Failed` event precedes this.
    Failed,
}

#[derive(Debug)]
pub enum DeckEvent {
    Started(TrackId),
    Ended(TrackId, EndReason),
    Failed(TrackId, PlaybackError),
    /// The track has been waiting for media for `waiting`; reported once per stall.
    Stuck(TrackId, Duration),
}

#[derive(Debug)]
pub struct Deck {
    tracks: Tracks,
    strip: Strip,
}

/// The deck's tracks and what happens to them; everything but the processing of their audio.
#[derive(Debug)]
struct Tracks {
    current: Option<Voice>,
    next: Option<Voice>,
    /// Voices sounding over the current track: clips mixed in (reported), and tails of tracks
    /// that fade out (no longer reported).
    others: Vec<Voice>,
    /// How long the end of a track overlaps the start of the next, in output time.
    crossfade: Ticks,
    stuck_after: Duration,
    events: Vec<DeckEvent>,
    /// Where the other voices render before they are added.
    scratch: Vec<f32>,
}

impl Deck {
    pub fn new(stuck_after: Duration) -> Self {
        Self {
            tracks: Tracks {
                current: None,
                next: None,
                others: Vec::new(),
                crossfade: 0,
                stuck_after,
                events: Vec::new(),
                scratch: Vec::new(),
            },
            strip: Strip::new(),
        }
    }

    /// Starts `track` now, replacing the current one.
    pub fn play(&mut self, track: DeckTrack) {
        self.tracks.forget_tails_if_idle();
        self.tracks.end_current(EndReason::Replaced);
        self.tracks.start(Voice::new(track));
    }

    /// Sets (or clears) the track that follows the current one; its download starts right away.
    /// With nothing playing, it starts immediately.
    pub fn set_next(&mut self, track: Option<DeckTrack>) {
        self.tracks.next = track.map(Voice::new);
        if self.tracks.current.is_none() {
            self.tracks.forget_tails_if_idle();
            self.tracks.advance();
        }
    }

    /// Ends the current track and starts the next one right away, if there is one.
    pub fn skip(&mut self) {
        let reason = if self.tracks.next.is_some() {
            EndReason::Replaced
        } else {
            EndReason::Stopped
        };
        self.tracks.finish_current(reason);
    }

    /// Stops the current track and forgets the next one.
    pub fn stop(&mut self) {
        self.tracks.next = None;
        self.tracks.end_current(EndReason::Stopped);
    }

    pub fn seek(&mut self, position: Ticks) -> Result<(), PlaybackError> {
        self.tracks.cut_tails();
        match &mut self.tracks.current {
            Some(voice) => voice.seek(position),
            None => Ok(()),
        }
    }

    /// Changes where the current track stops; `None` plays it to the end.
    pub fn set_end_at(&mut self, end_at: Option<Ticks>) {
        if let Some(voice) = &mut self.tracks.current {
            voice.track.end_at = end_at;
        }
    }

    /// How long the end of each track overlaps the start of the next, in output time (with a
    /// playback rate, more media); 0 plays them back to back. At most half of a track fades.
    pub fn set_crossfade(&mut self, crossfade: Ticks) {
        self.tracks.crossfade = crossfade.max(0);
    }

    /// How many frames of the tracks one output frame takes: 1 unless the effects set a rate.
    pub fn rate(&self) -> f32 {
        self.strip.rate()
    }

    /// Replaces the effects (`None` removes them) from the next render on.
    pub fn set_effects(&mut self, effects: Option<Effects>) {
        self.strip.set_effects(effects);
    }

    /// Linear gain applied after the effects; changes take a few milliseconds.
    pub fn set_volume(&mut self, volume: f32) {
        self.strip.set_volume(volume);
    }

    /// Clears the effects' state, such as reverb tails.
    pub fn reset_effects(&mut self) {
        self.strip.reset();
    }

    pub fn current(&self) -> Option<&DeckTrack> {
        self.tracks.current.as_ref().map(|voice| &voice.track)
    }

    pub fn next(&self) -> Option<&DeckTrack> {
        self.tracks.next.as_ref().map(|voice| &voice.track)
    }

    /// Whether [`place`](Self::place) would take a clip: in [`ClipMode::Queue`], one waits at
    /// most, so with a clip playing and one next there is no room.
    pub fn has_room(&self, mode: ClipMode) -> bool {
        mode != ClipMode::Queue || self.tracks.current.is_none() || self.tracks.next.is_none()
    }

    /// Plays a clip as `mode` says; one there is no room for (see [`has_room`](Self::has_room))
    /// is given back.
    pub fn place(&mut self, clip: Clip, mode: ClipMode) -> Result<Placed, Box<Clip>> {
        if !self.has_room(mode) {
            return Err(Box::new(clip));
        }
        let tracks = &mut self.tracks;
        match mode {
            ClipMode::Queue if tracks.current.is_some() => {
                tracks.next = Some(Voice::clip(clip));
                return Ok(Placed::Next);
            }
            ClipMode::Queue => {
                tracks.forget_tails_if_idle();
                tracks.start(Voice::clip(clip));
            }
            ClipMode::Mix(most) => {
                tracks.forget_tails_if_idle();
                while tracks.others.iter().filter(|v| v.reported).count() >= most.max(1) {
                    let oldest = tracks.others.iter().find(|v| v.reported).map(Voice::id);
                    tracks.end_other(oldest.expect("counted above"), EndReason::Replaced, None);
                }
                tracks.events.push(DeckEvent::Started(clip.track.id));
                tracks.others.push(Voice::clip(clip));
            }
            ClipMode::Replace => {
                tracks.end_all(EndReason::Replaced, None);
                tracks.forget_tails_if_idle();
                tracks.start(Voice::clip(clip));
            }
        }
        Ok(Placed::Playing)
    }

    /// Ends the track or clip `id` (current, mixed in, or next), fading out over `fade` frames or
    /// as it fades when cut off; the next one in line starts. Returns whether it was there. A
    /// next track has not started, so no event says it ended.
    pub fn end(&mut self, id: TrackId, reason: EndReason, fade: Option<usize>) -> bool {
        let tracks = &mut self.tracks;
        if tracks.current.as_ref().is_some_and(|v| v.id() == id) {
            tracks.end_current_fading(reason, fade);
            tracks.advance();
            true
        } else if tracks.next.as_ref().is_some_and(|v| v.id() == id) {
            tracks.next = None;
            true
        } else {
            tracks.end_other(id, reason, fade)
        }
    }

    /// Ends everything the deck plays, fading out over `fade` frames or as each fades when cut
    /// off, and forgets what comes next.
    pub fn end_all(&mut self, reason: EndReason, fade: Option<usize>) {
        self.tracks.end_all(reason, fade);
    }

    /// Where the track or clip `id` is (current, mixed in, or next), if the deck holds it.
    pub fn position_of(&self, id: TrackId) -> Option<Ticks> {
        let tracks = &self.tracks;
        tracks
            .current
            .iter()
            .chain(&tracks.others)
            .chain(&tracks.next)
            .find(|voice| voice.id() == id)
            .map(|voice| voice.track.player.position())
    }

    /// Whether a track or clip it reports on had audio in the last render, i.e. one is playing
    /// and not waiting for media.
    pub fn is_sounding(&self) -> bool {
        let tracks = &self.tracks;
        tracks
            .current
            .iter()
            .chain(&tracks.others)
            .any(|v| v.reported && v.sounding)
    }

    /// Whether no track is playing (tails of cut-off tracks may still be fading out).
    pub fn is_idle(&self) -> bool {
        self.tracks.current.is_none()
    }

    /// Whether rendering would output anything but silence (or effect tails).
    pub fn is_audible(&self) -> bool {
        self.tracks.current.is_some() || !self.tracks.others.is_empty()
    }

    /// The players of every track the deck holds, e.g. for their cache usage.
    pub fn players(&self) -> impl Iterator<Item = &Player> {
        let tracks = &self.tracks;
        tracks
            .current
            .iter()
            .chain(&tracks.others)
            .chain(&tracks.next)
            .map(|voice| &voice.track.player)
    }

    pub fn drain_events(&mut self) -> Vec<DeckEvent> {
        std::mem::take(&mut self.tracks.events)
    }

    /// Fills `out` (interleaved stereo) completely: track audio where there is any, silence
    /// elsewhere, through the effects. Never blocks.
    pub fn render(&mut self, out: &mut [f32]) {
        let rate = self.strip.rate();
        let tracks = &mut self.tracks;
        self.strip.render(out, |input| tracks.render(input, rate));
    }
}

impl Tracks {
    /// Fills `out` with the tracks' audio, moving on to the next track where one ends, and with
    /// silence where there is nothing to play.
    fn render(&mut self, out: &mut [f32], rate: f32) {
        let frames = out.len() / CHANNELS;
        let mut filled = 0;
        while filled < frames {
            self.crossfade_if_due(rate);
            let Some(voice) = &mut self.current else {
                break;
            };
            let id = voice.id();
            let (rendered, status) = voice.render(&mut out[filled * CHANNELS..]);
            filled += rendered;
            if let Some(waiting) = voice.check_stuck(self.stuck_after) {
                self.events.push(DeckEvent::Stuck(id, waiting));
            }
            match status {
                Status::Playing => {}
                Status::Ended => self.finish_current(EndReason::Finished),
                Status::Buffering => break,
                Status::Failed(e) => {
                    self.events.push(DeckEvent::Failed(id, e));
                    self.finish_current(EndReason::Failed);
                }
            }
        }
        out[filled * CHANNELS..].fill(0.0);
        self.render_others(out);
    }

    /// Adds the voices sounding over the current track to `out`, and lets go of finished ones.
    fn render_others(&mut self, out: &mut [f32]) {
        if self.others.is_empty() {
            return;
        }
        if self.scratch.len() < out.len() {
            self.scratch.resize(out.len(), 0.0);
        }
        let scratch = &mut self.scratch[..out.len()];
        let (stuck_after, events) = (self.stuck_after, &mut self.events);
        self.others.retain_mut(|voice| {
            let status = voice.render_add(scratch, out);
            if !voice.reported {
                // a tail lasts until it has faded out, or has nothing more to give
                return matches!(status, Status::Playing) && !voice.is_faded_out();
            }
            if let Some(waiting) = voice.check_stuck(stuck_after) {
                events.push(DeckEvent::Stuck(voice.id(), waiting));
            }
            match status {
                Status::Playing | Status::Buffering => true,
                Status::Ended => {
                    events.push(DeckEvent::Ended(voice.id(), EndReason::Finished));
                    false
                }
                Status::Failed(e) => {
                    events.push(DeckEvent::Failed(voice.id(), e));
                    events.push(DeckEvent::Ended(voice.id(), EndReason::Failed));
                    false
                }
            }
        });
    }

    /// Starts the next track over the end of the current one, once that end is a crossfade
    /// away and the next track is downloaded far enough not to stall right away.
    fn crossfade_if_due(&mut self, rate: f32) {
        let (Some(voice), Some(next)) = (&self.current, &self.next) else {
            return;
        };
        let next = &next.track;
        if self.crossfade == 0 {
            return;
        }
        let track = &voice.track;
        let Some(end) = track
            .end_at
            .or_else(|| track.player.media_end())
            .or(track.length)
        else {
            return; // a stream: nothing to fade into before it ends
        };
        // in the tracks' time, which a rate makes pass faster than the output's
        let media = |ticks: Ticks| (ticks as f64 * f64::from(rate)) as Ticks;
        let fade = media(self.crossfade).min((end - track.start).max(0) / 2);
        let remaining = end - track.player.position();
        if remaining > fade || remaining < media(MIN_CROSSFADE) || !is_ready(&next.player) {
            return;
        }
        let frames = usize::try_from(remaining).unwrap_or(0);
        tracing::debug!(
            fade_ms = (remaining as f64 / f64::from(rate) / 48.0) as u64,
            "crossfading into the next track"
        );
        let mut outgoing = self.current.take().expect("checked above");
        self.events
            .push(DeckEvent::Ended(outgoing.id(), EndReason::Finished));
        outgoing.fade_out(frames, Curve::EqualPower);
        self.push_tail(outgoing);
        let mut next = self.next.take().expect("checked above");
        next.fade_in(frames, Curve::EqualPower);
        self.start(next);
    }

    /// Ends the current track and moves on to the next one, if any.
    fn finish_current(&mut self, reason: EndReason) {
        self.end_current(reason);
        self.advance();
    }

    fn end_current(&mut self, reason: EndReason) {
        self.end_current_fading(reason, None);
    }

    /// Ends the current track, fading out over `fade` frames or as it fades when cut off.
    fn end_current_fading(&mut self, reason: EndReason, fade: Option<usize>) {
        let Some(mut voice) = self.current.take() else {
            return;
        };
        self.events.push(DeckEvent::Ended(voice.id(), reason));
        // a track that played to its end (or failed) has nothing left to fade out
        if matches!(reason, EndReason::Replaced | EndReason::Stopped) {
            self.cut_tails();
            voice.cut(fade);
            self.push_tail(voice);
        }
    }

    /// Ends a clip mixed in over the current track; returns whether there was one.
    fn end_other(&mut self, id: TrackId, reason: EndReason, fade: Option<usize>) -> bool {
        let Some(index) = self.others.iter().position(|v| v.reported && v.id() == id) else {
            return false;
        };
        let mut voice = self.others.remove(index);
        self.events.push(DeckEvent::Ended(id, reason));
        voice.cut(fade);
        self.push_tail(voice);
        true
    }

    fn end_all(&mut self, reason: EndReason, fade: Option<usize>) {
        self.next = None;
        self.end_current_fading(reason, fade);
        let mixed: Vec<TrackId> = self
            .others
            .iter()
            .filter(|v| v.reported)
            .map(Voice::id)
            .collect();
        for id in mixed {
            self.end_other(id, reason, fade);
        }
    }

    fn push_tail(&mut self, voice: Voice) {
        let tails = self.others.iter().filter(|v| !v.reported).count();
        if tails == MAX_TAILS
            && let Some(oldest) = self.others.iter().position(|v| !v.reported)
        {
            self.others.remove(oldest);
        }
        self.others.push(voice);
    }

    /// Fades out what is fading quickly instead, e.g. a crossfade's old track on a skip.
    fn cut_tails(&mut self) {
        for voice in self.others.iter_mut().filter(|voice| !voice.reported) {
            voice.fade_out(DECLICK_FRAMES, Curve::Linear);
        }
    }

    /// Tails that never got to fade out, because the deck was not rendered meanwhile, must not
    /// sound in front of what plays next.
    fn forget_tails_if_idle(&mut self) {
        if self.current.is_none() {
            self.others.retain(|voice| voice.reported);
        }
    }

    fn advance(&mut self) {
        if let Some(next) = self.next.take() {
            self.start(next);
        }
    }

    fn start(&mut self, voice: Voice) {
        self.events.push(DeckEvent::Started(voice.id()));
        self.current = Some(voice);
    }
}

/// Whether a track is downloaded far enough ahead to start playing without stalling at once.
fn is_ready(player: &Player) -> bool {
    let ahead = player.buffered_ahead();
    ahead >= READY_AHEAD
        || player
            .media_end()
            .is_some_and(|end| player.position() + ahead >= end)
}
