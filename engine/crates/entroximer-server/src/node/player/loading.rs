//! Loading the queries of a queue's loading items, in the background.
//!
//! `queue.add` answers right away: each query becomes a loading item in its place, which this
//! loads, in queue order, a few at a time; a query added on its own (a bot's `/play`) loads at
//! once instead, as `tracks.load` would. A loaded item is replaced in place by what its query
//! found (see [`Queue::replace`]), so the queue keeps the order the client asked for whichever
//! load finishes first. The first item of the queue loads ahead of the others: its yt-dlp runs take
//! normal turns, the others background turns (see [`background`]), so that playback starts after
//! one load and never waits behind a long list. A playlist is one load: one listing of all its
//! tracks, from its start.
//!
//! An item that fails to load is reported with `trackError` and removed. One refused for too many
//! requests stays, and background loads wait a while before trying again (see [`Backoff`]).
//! Loads of items that leave the queue are cancelled.
//!
//! [`Backoff`]: crate::node::backoff::Backoff

use std::time::Instant;

use super::*;
use crate::node::queue::Pending;
use crate::node::tracks::{load_error, to_track};
use crate::sources::background::{self, Promotion};
use crate::sources::{Listing, LoadOutcome, PlaylistInfo};

/// Loads one player runs at once: its first item's, and two in the background. Every player
/// shares the background turns, first come first served, so this also keeps one long list from
/// holding them all.
const LOADS_PER_PLAYER: usize = 3;
/// A single link is resolved to be described; that is kept for playing if its item is this close
/// to the front of the queue, and dropped otherwise (it would expire before it plays).
const KEEP_WITHIN: usize = 2;
/// Attempts at a load that the source refuses for too many requests.
const MAX_ATTEMPTS: u32 = 3;

/// A loading item's load.
pub(super) struct Load {
    /// Attempts so far, the running one included.
    attempts: u32,
    /// Tells the running attempt's outcome from an earlier one's.
    attempt: u64,
    promotion: Promotion,
    /// `None` between attempts, while waiting out rate limiting.
    task: Option<AbortHandle>,
}

impl Load {
    #[cfg(test)]
    pub(super) fn promoted(&self) -> bool {
        self.promotion.is_promoted()
    }
}

/// What an item's query found.
#[derive(Default)]
pub(super) struct Found {
    pub(super) tracks: Vec<Track>,
    /// A single link's track, resolved to describe it.
    pub(super) resolved: Option<Loaded>,
    pub(super) playlist: Option<PlaylistInfo>,
}

impl Found {
    fn one(track: Track) -> Self {
        Self {
            tracks: vec![track],
            ..Self::default()
        }
    }
}

impl GuildPlayer {
    /// Starts loading the loading items that are due, and cancels loads of items that left the
    /// queue. Called with the state locked, right after it changed, and regularly.
    pub(super) fn plan_loads(&self, state: &mut State) {
        if state.destroying {
            return;
        }
        let Some(node) = self.node.upgrade() else {
            return;
        };
        let State { queue, loads, .. } = &mut *state;
        loads.retain(|id, load| {
            let queued = queue.get(id).is_some_and(QueueItem::is_loading);
            if !queued && let Some(task) = &load.task {
                task.abort();
            }
            queued
        });

        let waiting_out = node.load_backoff.is_waiting(Instant::now());
        let mut running = loads.values().filter(|load| load.task.is_some()).count();
        let mut due = Vec::new();
        for (position, item) in queue.iter().enumerate() {
            let Some(pending) = item.pending() else {
                continue;
            };
            // the first item, and one a client waits for, load at once with normal turns
            let urgent = position == 0 || pending.now;
            if let Some(load) = loads.get(&item.id)
                && load.task.is_some()
            {
                if urgent {
                    load.promotion.promote();
                }
                continue;
            }
            // unless refused before; the others in queue order, a few at a time
            let retrying = loads.contains_key(&item.id);
            let waits = waiting_out && (retrying || !urgent);
            if waits || (!urgent && running >= LOADS_PER_PLAYER) {
                continue;
            }
            due.push((item.clone(), urgent));
            running += 1;
        }
        for (item, urgent) in due {
            self.start_load(state, &node, item, urgent);
        }
    }

    /// Cancels every load, e.g. as the player goes.
    pub(super) fn cancel_loads(&self, state: &mut State) {
        for (_, load) in state.loads.drain() {
            if let Some(task) = load.task {
                task.abort();
            }
        }
    }

    fn start_load(&self, state: &mut State, node: &Arc<Node>, item: QueueItem, urgent: bool) {
        let Some(pending) = item.pending().cloned() else {
            return;
        };
        let attempt = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        let load = state.loads.entry(item.id.clone()).or_insert_with(|| Load {
            attempts: 0,
            attempt,
            promotion: Promotion::default(),
            task: None,
        });
        load.attempts += 1;
        load.attempt = attempt;
        if urgent {
            load.promotion.promote();
        }
        tracing::debug!(
            guild_id = %self.guild_id,
            item = %item.id,
            query = %item.track.info.uri,
            urgent,
            attempt = load.attempts,
            "loading a queued query"
        );
        let promotion = load.promotion.clone();
        let (node, player, item_id) = (Arc::clone(node), self.me.clone(), item.id);
        let task = tokio::spawn(async move {
            let found = background::run(promotion, find(&node, pending)).await;
            if let Some(player) = player.upgrade() {
                player.finish_load(&node, &item_id, attempt, found);
            }
        });
        load.task = Some(task.abort_handle());
    }

    fn finish_load(
        &self,
        node: &Node,
        item_id: &str,
        attempt: u64,
        found: Result<Found, ApiError>,
    ) {
        {
            let mut state = self.state();
            let Some(attempts) = state
                .loads
                .get(item_id)
                .filter(|load| load.attempt == attempt)
                .map(|load| load.attempts)
            else {
                return; // cancelled meanwhile
            };
            let Some(item) = state.queue.get(item_id).cloned() else {
                state.loads.remove(item_id);
                return;
            };
            let refused = found
                .as_ref()
                .is_err_and(|e| matches!(e.code, ErrorCode::RateLimited | ErrorCode::BotCheck));
            let skip_went_on = match found {
                Ok(found) => {
                    state.loads.remove(item_id);
                    node.load_backoff.got_through();
                    self.loaded(&mut state, node, &item, found);
                    false
                }
                Err(error) if refused && attempts < MAX_ATTEMPTS => {
                    if let Some(load) = state.loads.get_mut(item_id) {
                        load.task = None;
                    }
                    let wait = node.load_backoff.refused(Instant::now());
                    tracing::warn!(
                        guild_id = %self.guild_id,
                        item = %item.id,
                        "a queued query was refused ({}); background loads wait {} s: {}",
                        serde_json::to_value(error.code).unwrap_or_default(),
                        wait.as_secs(),
                        error.message
                    );
                    false
                }
                Err(error) => {
                    state.loads.remove(item_id);
                    tracing::debug!(
                        guild_id = %self.guild_id,
                        item = %item.id,
                        "cannot load a queued query: {}",
                        error.message
                    );
                    self.emit(Event::TrackError {
                        guild_id: self.guild_id,
                        track: item.track.clone(),
                        item_id: item.id.clone(),
                        error,
                    });
                    let change = state.queue.remove(std::slice::from_ref(&item.id));
                    self.report(&state.queue, change);
                    self.continue_waiting_skip(&mut state, item_id)
                }
            };
            self.plan_next(&mut state);
            // a skip that went on reports the end of the queue itself, once its track ends
            if !skip_went_on {
                self.end_if_ran_out(&state);
            }
        }
        self.drain_deck();
        self.sync_output();
    }

    /// Puts what `item`'s query found in its place.
    pub(super) fn loaded(&self, state: &mut State, node: &Node, item: &QueueItem, found: Found) {
        let Found {
            mut tracks,
            resolved,
            playlist,
        } = found;
        let user_data = item.track.user_data.as_ref();
        // the item itself makes room
        let room = MAX_QUEUE_LENGTH + 1 - state.queue.len().min(MAX_QUEUE_LENGTH);
        if tracks.len() > room {
            tracing::info!(
                guild_id = %self.guild_id,
                left_out = tracks.len() - room,
                "the queue is full; a playlist's tracks are left out"
            );
            tracks.truncate(room);
        }
        let mut items: Vec<QueueItem> = tracks
            .into_iter()
            .map(|track| {
                QueueItem::new(Track {
                    user_data: user_data.cloned(),
                    ..track
                })
            })
            .collect();
        if let Some(first) = items.first_mut() {
            first.id = item.id.clone();
        }
        // as `tracks.load` would keep it, for a load a client waits for
        let waited_for = item.pending().is_some_and(|pending| pending.now);
        let near = waited_for
            || state
                .queue
                .position(&item.id)
                .is_some_and(|p| p < KEEP_WITHIN);
        if near && let Some(resolved) = resolved {
            node.tracks.keep(&resolved.track.encoded, &resolved);
        }
        let change = state.queue.replace(&item.id, items, playlist);
        self.report(&state.queue, change);
        self.continue_waiting_skip(state, &item.id);
    }

    /// If a skip waits for the item `id`, which has loaded (keeping its id) or failed: prepares
    /// it, or skips on to the next one. Returns whether a skip waited.
    fn continue_waiting_skip(&self, state: &mut State, id: &str) -> bool {
        let waiting = state
            .preparing
            .as_ref()
            .is_some_and(|p| p.skip && p.task.is_none() && p.item_id == id);
        if !waiting {
            return false;
        }
        state.preparing = None;
        match state.queue.head().cloned() {
            Some(head) if head.id == id => self.start_preparation(state, head, true),
            _ => self.continue_skip(state),
        }
        true
    }
}

/// Loads what a loading item stands for.
async fn find(node: &Node, pending: Pending) -> Result<Found, ApiError> {
    let ctx = &node.ctx;
    let Pending {
        query,
        start_index,
        now,
    } = pending;
    let listing = Listing::Queue {
        start_index,
        limit: None,
    };
    let outcome = ctx
        .sources
        .load_with(&query, listing, ctx)
        .await
        .map_err(|e| load_error(&e))?;
    let found = match outcome {
        LoadOutcome::Track(resolved) => {
            let resolved = Loaded::from(resolved);
            Found {
                resolved: Some(resolved.clone()),
                ..Found::one(resolved.track)
            }
        }
        // resolved as `tracks.load` would, for a client waiting: a Spotify track is found on
        // YouTube now, so that one without a recording fails now
        LoadOutcome::Described(found) if now => {
            let resolved = ctx
                .sources
                .resolve(&found, ctx)
                .await
                .map_err(|e| load_error(&e))?;
            let resolved = Loaded::from(resolved);
            Found {
                resolved: Some(resolved.clone()),
                ..Found::one(resolved.track)
            }
        }
        LoadOutcome::Described(found) => Found::one(to_track(found)),
        LoadOutcome::Playlist { playlist, tracks } => Found {
            tracks: tracks.into_iter().map(to_track).collect(),
            resolved: None,
            playlist: Some(playlist),
        },
        LoadOutcome::Search(tracks) => Found {
            tracks: tracks.into_iter().take(1).map(to_track).collect(),
            ..Found::default()
        },
        LoadOutcome::Empty => Found::default(),
    };
    if found.tracks.is_empty() {
        return Err(ApiError::new(
            ErrorCode::NoResults,
            format!("nothing found for `{query}`"),
        ));
    }
    Ok(found)
}
