//! One continuous audio output: the playing track, an optional preloaded next track, effects and
//! volume.
//!
//! A deck never ends: with nothing to play it outputs silence. When the current track finishes,
//! the preloaded next track takes over within the same render call, so tracks follow each other
//! without a gap. Effects and volume belong to the deck, so they carry across tracks, and effect
//! tails (reverb, echo) ring out into whatever follows.
//!
//! Everything that happens is reported as [`DeckEvent`]s, drained by the owner.

use std::time::Duration;

use crate::dsp::{CHANNELS, GraphRunner};
use crate::player::{PlaybackError, Player, Status};
use crate::timeline::Ticks;

/// Identifies a track to the deck's owner; the deck only passes it back in events.
pub type TrackId = u64;

#[derive(Debug)]
pub struct DeckTrack {
    pub id: TrackId,
    pub player: Player,
    /// Stop when playback reaches this position.
    pub end_at: Option<Ticks>,
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
    current: Option<Playing>,
    next: Option<DeckTrack>,
    graph: Option<GraphRunner>,
    volume: f32,
    stuck_after: Duration,
    events: Vec<DeckEvent>,
}

#[derive(Debug)]
struct Playing {
    track: DeckTrack,
    stuck_reported: bool,
}

impl Deck {
    pub fn new(stuck_after: Duration) -> Self {
        Self {
            current: None,
            next: None,
            graph: None,
            volume: 1.0,
            stuck_after,
            events: Vec::new(),
        }
    }

    /// Starts `track` now, replacing the current one.
    pub fn play(&mut self, track: DeckTrack) {
        self.end_current(EndReason::Replaced);
        self.start(track);
    }

    /// Sets (or clears) the track that follows the current one; its download starts right away.
    /// With nothing playing, it starts immediately.
    pub fn set_next(&mut self, track: Option<DeckTrack>) {
        self.next = track;
        if self.current.is_none() {
            self.advance();
        }
    }

    /// Stops the current track and forgets the next one.
    pub fn stop(&mut self) {
        self.next = None;
        self.end_current(EndReason::Stopped);
    }

    pub fn seek(&mut self, position: Ticks) -> Result<(), PlaybackError> {
        match &mut self.current {
            Some(playing) => {
                playing.stuck_reported = false;
                playing.track.player.seek(position)
            }
            None => Ok(()),
        }
    }

    /// Changes where the current track stops; `None` plays it to the end.
    pub fn set_end_at(&mut self, end_at: Option<Ticks>) {
        if let Some(playing) = &mut self.current {
            playing.track.end_at = end_at;
        }
    }

    pub fn set_graph(&mut self, graph: Option<GraphRunner>) {
        self.graph = graph;
    }

    /// Linear gain applied after the effects.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
    }

    pub fn current(&self) -> Option<&DeckTrack> {
        self.current.as_ref().map(|p| &p.track)
    }

    pub fn next(&self) -> Option<&DeckTrack> {
        self.next.as_ref()
    }

    pub fn is_idle(&self) -> bool {
        self.current.is_none()
    }

    pub fn drain_events(&mut self) -> Vec<DeckEvent> {
        std::mem::take(&mut self.events)
    }

    /// Fills `out` (interleaved stereo) completely: track audio where there is any, silence
    /// elsewhere. Never blocks.
    pub fn render(&mut self, out: &mut [f32]) {
        let frames = out.len() / CHANNELS;
        let mut filled = 0;
        while filled < frames {
            let Some(playing) = &mut self.current else {
                break;
            };
            let player = &mut playing.track.player;
            let limit = match playing.track.end_at {
                Some(end) => usize::try_from(end - player.position()).unwrap_or(0),
                None => usize::MAX,
            };
            let wanted = (frames - filled).min(limit);
            if wanted == 0 {
                self.finish_current(EndReason::Finished);
                continue;
            }
            match player.render(&mut out[filled * CHANNELS..(filled + wanted) * CHANNELS]) {
                Ok(rendered) => {
                    filled += rendered.frames;
                    match rendered.status {
                        Status::Playing => {}
                        Status::Ended => self.finish_current(EndReason::Finished),
                        Status::Buffering => {
                            self.report_if_stuck();
                            break;
                        }
                    }
                }
                Err(e) => {
                    let id = playing.track.id;
                    self.events.push(DeckEvent::Failed(id, e));
                    self.finish_current(EndReason::Failed);
                }
            }
        }
        out[filled * CHANNELS..].fill(0.0);

        if let Some(graph) = &mut self.graph {
            graph.process(out);
        }
        if self.volume != 1.0 {
            for sample in out.iter_mut() {
                *sample *= self.volume;
            }
        }
    }

    fn report_if_stuck(&mut self) {
        let Some(playing) = &mut self.current else {
            return;
        };
        let waiting = playing
            .track
            .player
            .buffering_since()
            .map(|since| since.elapsed());
        match waiting {
            Some(waiting) if waiting >= self.stuck_after && !playing.stuck_reported => {
                playing.stuck_reported = true;
                self.events
                    .push(DeckEvent::Stuck(playing.track.id, waiting));
            }
            Some(_) => {}
            None => playing.stuck_reported = false,
        }
    }

    /// Ends the current track and moves on to the next one, if any.
    fn finish_current(&mut self, reason: EndReason) {
        self.end_current(reason);
        self.advance();
    }

    fn end_current(&mut self, reason: EndReason) {
        if let Some(playing) = self.current.take() {
            self.events.push(DeckEvent::Ended(playing.track.id, reason));
        }
    }

    fn advance(&mut self) {
        if let Some(next) = self.next.take() {
            self.start(next);
        }
    }

    fn start(&mut self, track: DeckTrack) {
        self.events.push(DeckEvent::Started(track.id));
        self.current = Some(Playing {
            track,
            stuck_reported: false,
        });
    }
}
