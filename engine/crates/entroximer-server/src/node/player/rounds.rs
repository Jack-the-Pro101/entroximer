//! A repeated queue's next round: tracks that played (to their end, or skipped) are kept in the
//! engine's loop cache (see `node::loop_cache`) if their media is on disk as far as they play it
//! (from their start time to their end time), and play from there when they come round again,
//! without being loaded again. The player holds the tracks it kept while its queue repeats and
//! has them, and lets go of the others. Files on the engine's own disk (local files and uploads)
//! are not kept: they load again for free, and a kept upload would keep its file from expiring.

use std::collections::HashSet;

use entroximer_core::player::Kept;

use super::*;
use crate::api::protocol::Times;
use crate::node::loop_cache::Holder;
use crate::sources::{local, upload};

impl GuildPlayer {
    /// Who holds this player's kept tracks.
    fn holder(&self) -> Holder {
        (self.user_id, self.guild_id)
    }

    /// Keeps `media`, which `item` played, for the queue's next round, if it repeats and has
    /// the item again.
    pub(super) fn keep_for_next_round(&self, state: &mut State, item: &QueueItem, media: &Kept) {
        let repeats =
            state.queue.repeat() == Repeat::Queue && state.queue.position(&item.id).is_some();
        if !repeats || plays_from_disk(&item.track) {
            return;
        }
        let Some(node) = self.node.upgrade() else {
            return;
        };
        let key = &item.track.encoded;
        let kept = node.loop_cache.offer(
            key,
            media,
            Times::of(&item.track),
            plays_from_youtube(&item.track),
            self.holder(),
        );
        if kept {
            state.kept.insert(key.clone());
        }
    }

    /// `item` as kept from an earlier round (of any player's queue), ready to play, if what was
    /// kept holds the part it plays now.
    pub(super) fn kept_track(&self, item: &QueueItem) -> Option<DeckTrack> {
        let times = Times::of(&item.track);
        let kept = self
            .node
            .upgrade()?
            .loop_cache
            .get(&item.track.encoded, times)?;
        Some(DeckTrack {
            id: self.next_track.fetch_add(1, Ordering::Relaxed),
            // media kept only up to the item's end time does not tell its length
            length: kept
                .length()
                .or(item.track.info.length.map(Millis::to_ticks)),
            player: Player::from_kept(Handle::current(), &kept, times.start_ticks()),
            start: times.start_ticks().unwrap_or(0),
            end_at: times.end_ticks(),
        })
    }

    /// Lets go of kept tracks that the queue no longer has, or all of them once it no longer
    /// repeats. Called with the state locked, right after it changed.
    pub(super) fn release_kept(&self, state: &mut State) {
        if state.kept.is_empty() || state.kept_version == state.queue.version() {
            return;
        }
        state.kept_version = state.queue.version();
        let Some(node) = self.node.upgrade() else {
            return;
        };
        let holder = self.holder();
        let State {
            kept, queue, items, ..
        } = &mut *state;
        // what plays or is prepared has left the queue for now
        let wanted: HashSet<&str> = if queue.repeat() == Repeat::Queue {
            queue
                .iter()
                .chain(items.values())
                .map(|item| item.track.encoded.as_str())
                .collect()
        } else {
            HashSet::new()
        };
        kept.retain(|key| {
            let still = wanted.contains(key.as_str());
            if !still {
                node.loop_cache.release(key, holder);
            }
            still
        });
    }

    /// Lets go of every kept track, as the player goes.
    pub(super) fn release_all_kept(&self, state: &mut State) {
        let Some(node) = self.node.upgrade() else {
            return;
        };
        for key in state.kept.drain() {
            node.loop_cache.release(&key, self.holder());
        }
    }
}

/// Whether the track's audio comes from YouTube, whose tracks the loop cache keeps first: a
/// Spotify track plays from its recording there.
fn plays_from_youtube(track: &Track) -> bool {
    matches!(track.info.source_name.as_str(), "youtube" | "spotify")
}

/// Whether the track plays from a file on the engine's own disk.
fn plays_from_disk(track: &Track) -> bool {
    [local::SOURCE, upload::SOURCE].contains(&track.info.source_name.as_str())
}
