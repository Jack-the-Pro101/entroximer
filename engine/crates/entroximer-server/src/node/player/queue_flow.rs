//! How the queue plays: what comes next, preparing it, skipping, and following the deck.
//!
//! # Seamless queues
//! The item that comes next is prepared in the deck's gapless next slot: resolved and
//! downloading. A tick checks how much of the current track remains and starts preparing once
//! that drops below the preload lead, so the next track is buffered long before it is needed,
//! whatever the client does meanwhile. If the queue changes so that another item comes next, the
//! prepared one is dropped, which cancels its download. When the deck switches to the prepared
//! track, its item leaves the queue. An item that comes again right after itself (a repeated
//! track) is not loaded again: it plays again from what the current track downloaded.
//!
//! A queue item that cannot be loaded is reported with `trackError` and removed, and the queue
//! goes on; after [`MAX_FAILURES`] in a row it waits for the client to change something. A
//! loading item (see `loading`) is prepared once it has loaded; a skip to one waits for that.

use super::*;

impl GuildPlayer {
    pub(super) fn skip_locked(&self, state: &mut State, to: Option<&str>) -> Result<(), ApiError> {
        if let Some(to) = to {
            let position = state.queue.position(to).ok_or_else(|| no_item(to))?;
            let before: Vec<String> = state
                .queue
                .iter()
                .take(position)
                .map(|item| item.id.clone())
                .collect();
            let change = state.queue.remove(&before);
            self.report(&state.queue, change);
        }
        state.halted = false;
        state.failures = 0;
        state.finished = None;
        let repeat_queue = state.queue.repeat() == Repeat::Queue;

        // the item skipped: the one an earlier skip is still waiting for, else the current one
        match state.preparing.take_if(|p| p.skip) {
            Some(pending) if Some(pending.item_id.as_str()) == to => {
                // on its way already
                state.preparing = Some(pending);
                return Ok(());
            }
            Some(pending) => {
                pending.cancel();
                let change = if repeat_queue {
                    state.queue.move_item(&pending.item_id, usize::MAX)
                } else {
                    state.queue.remove(std::slice::from_ref(&pending.item_id))
                };
                self.report(&state.queue, change);
            }
            None => {
                let current = state.current.and_then(|id| state.items.get(&id)).cloned();
                if repeat_queue && let Some(current) = current {
                    let change = state.queue.add(vec![current], None);
                    self.report(&state.queue, change);
                }
            }
        }

        let head = state.queue.head().cloned();
        let head_prepared = head.as_ref().is_some_and(|head| {
            state
                .prepared
                .as_ref()
                .is_some_and(|(_, item)| *item == head.id)
        });
        match head {
            Some(_) if head_prepared => self.deck().skip(),
            Some(head) => {
                self.drop_prepared(state);
                self.start_preparation(state, head, true);
            }
            None => {
                self.drop_prepared(state);
                self.deck().skip();
            }
        }
        // the skip's target loads first, and the items skipped stop loading
        self.plan_loads(state);
        Ok(())
    }

    /// The client stopped playback: the queue waits until it asks for something to play.
    pub(super) fn halt(&self) {
        let mut state = self.state();
        state.halted = true;
        state.finished = None;
        self.drop_prepared(&mut state);
        self.deck().stop();
    }

    /// Prepares the item that comes next once the current track nears its end, and drops a
    /// prepared or preparing item that no longer comes next. Called with the state locked,
    /// right after it changed.
    pub(super) fn plan_next(&self, state: &mut State) {
        self.plan_loads(state);
        self.release_kept(state);
        if state.destroying || state.halted || !self.caught_up(state) {
            return;
        }
        if state.preparing.as_ref().is_some_and(|p| p.skip) {
            return; // the skip decides what plays
        }
        let anchor = state
            .current
            .and_then(|id| state.items.get(&id))
            .or(state.finished.as_ref());
        let wanted = state.queue.upcoming(anchor).cloned();
        let wanted_id = wanted.as_ref().map(|item| item.id.as_str());
        let prepared = state.prepared.as_ref().map(|(_, item)| item.as_str());
        let preparing = state.preparing.as_ref().map(|p| p.item_id.as_str());
        if wanted_id.is_some() && (prepared == wanted_id || preparing == wanted_id) {
            return;
        }
        self.drop_prepared(state);
        if let Some(wanted) = wanted
            && state.failures < MAX_FAILURES
            && self.preparation_due(state)
        {
            self.start_preparation(state, wanted, false);
        }
    }

    /// Whether the state has caught up with the deck, i.e. no switch of tracks is still on its
    /// way to [`Self::handle`] (which plans again once it arrives).
    fn caught_up(&self, state: &State) -> bool {
        let deck = self.deck();
        deck.current().map(|t| t.id) == state.current
            && deck.next().map(|t| t.id) == state.prepared.as_ref().map(|(id, _)| *id)
    }

    /// Whether the current track ends within the preload lead (plus a crossfade, which starts
    /// that much earlier), or nothing plays. A stream's end cannot be foreseen; what follows it is
    /// prepared once it ends.
    fn preparation_due(&self, state: &State) -> bool {
        let Some(node) = self.node.upgrade() else {
            return false;
        };
        let deck = self.deck();
        let Some(playing) = deck.current() else {
            return true;
        };
        let length = state
            .items
            .get(&playing.id)
            .and_then(|item| item.track.info.length)
            .map(Millis::to_ticks);
        let end = playing
            .end_at
            .or_else(|| playing.player.media_end())
            .or(length);
        // a crossfade lasts that long in output time, which a rate makes more of the track
        let crossfade = state.queue.crossfade().to_ticks() as f64 * f64::from(deck.rate());
        let lead = ticks(node.settings.preload_lead) + crossfade as Ticks;
        end.is_some_and(|end| end - playing.player.position() <= lead)
    }

    /// Resolves `item` in the background; see [`Self::finish_preparation`]. A loading item is
    /// prepared once it has loaded: a skip waits for that, and otherwise planning the next item
    /// does again then.
    pub(super) fn start_preparation(&self, state: &mut State, item: QueueItem, skip: bool) {
        let Some(node) = self.node.upgrade() else {
            return;
        };
        if let Some(previous) = state.preparing.take() {
            previous.cancel();
        }
        if item.is_loading() {
            if skip {
                state.preparing = Some(Preparation {
                    item_id: item.id,
                    attempt: self.next_attempt.fetch_add(1, Ordering::Relaxed),
                    skip,
                    task: None,
                });
            }
            return;
        }
        if let Some(replay) = self.replay(state, &item) {
            tracing::debug!(
                guild_id = %self.guild_id,
                item = %item.id,
                title = %item.track.info.title,
                skip,
                "replaying the current track"
            );
            self.set_prepared(state, item, replay, skip);
            return;
        }
        if let Some(kept) = self.kept_track(&item) {
            tracing::debug!(
                guild_id = %self.guild_id,
                item = %item.id,
                title = %item.track.info.title,
                skip,
                "playing the next track from the loop cache"
            );
            self.set_prepared(state, item, kept, skip);
            return;
        }
        let attempt = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            guild_id = %self.guild_id,
            item = %item.id,
            title = %item.track.info.title,
            skip,
            "preparing the next track"
        );
        let item_id = item.id.clone();
        let player = self.me.clone();
        let task = tokio::spawn(async move {
            let loaded = node.tracks.get(&node.ctx, &item.track.encoded).await;
            if let Some(player) = player.upgrade() {
                player.finish_preparation(&node, item, attempt, loaded);
            }
        });
        state.preparing = Some(Preparation {
            item_id,
            attempt,
            skip,
            task: Some(task.abort_handle()),
        });
    }

    fn finish_preparation(
        &self,
        node: &Node,
        mut item: QueueItem,
        attempt: u64,
        loaded: Result<Loaded, ApiError>,
    ) {
        {
            let mut state = self.state();
            let Some(preparation) = state.preparing.take_if(|p| p.attempt == attempt) else {
                return; // cancelled meanwhile
            };
            match loaded {
                Ok(loaded) => {
                    // resolving can tell more, e.g. a YouTube Music result's artist and length
                    let track = Track {
                        user_data: item.track.user_data.take(),
                        ..loaded.track.clone()
                    };
                    let change = state.queue.update(&item.id, track.clone());
                    self.report(&state.queue, change);
                    item.track = track;
                    let deck_track = self.deck_track(node, loaded, None, None);
                    self.set_prepared(&mut state, item, deck_track, preparation.skip);
                }
                Err(error) => {
                    state.failures += 1;
                    tracing::debug!(
                        guild_id = %self.guild_id,
                        item = %item.id,
                        "cannot load a queued track: {}",
                        error.message
                    );
                    self.emit(Event::TrackError {
                        guild_id: self.guild_id,
                        track: item.track.clone(),
                        item_id: item.id.clone(),
                        error,
                    });
                    let change = if state.queue.position(&item.id).is_some() {
                        state.queue.remove(std::slice::from_ref(&item.id))
                    } else {
                        // the item being repeated cannot be loaded again
                        state.queue.set_repeat(Repeat::Off)
                    };
                    self.report(&state.queue, change);
                    if preparation.skip {
                        self.continue_skip(&mut state);
                    } else {
                        self.plan_next(&mut state);
                        self.end_if_ran_out(&state);
                    }
                }
            }
        }
        self.drain_deck();
        self.sync_output();
    }

    /// The track playing `item` again from what it downloaded, if `item` is the one playing
    /// (a repeated track), so that it is not loaded again. Not for a stream, whose start is gone.
    fn replay(&self, state: &State, item: &QueueItem) -> Option<DeckTrack> {
        let deck = self.deck();
        let current = deck.current()?;
        let playing = state.items.get(&current.id)?;
        if playing.id != item.id || current.player.caps().live {
            return None;
        }
        Some(DeckTrack {
            id: self.next_track.fetch_add(1, Ordering::Relaxed),
            length: current.length,
            player: current.player.replay(),
            end_at: None,
        })
    }

    /// Makes `track`, which plays `item`, the deck's next track, or plays it now for a skip.
    fn set_prepared(&self, state: &mut State, item: QueueItem, track: DeckTrack, skip: bool) {
        state.failures = 0;
        state.prepared = Some((track.id, item.id.clone()));
        state.media.insert(track.id, track.player.keep());
        state.items.insert(track.id, item);
        let mut deck = self.deck();
        if skip {
            deck.play(track);
        } else {
            deck.set_next(Some(track));
        }
    }

    /// The item a skip waited for failed: skip to the next one, or stop.
    pub(super) fn continue_skip(&self, state: &mut State) {
        match state.queue.head().cloned() {
            Some(head) if state.failures < MAX_FAILURES => {
                self.start_preparation(state, head, true);
            }
            // the playing track ends, which reports the end of the queue
            _ if !self.deck().is_idle() => self.deck().skip(),
            _ => self.end_if_ran_out(state),
        }
    }

    /// Forgets the prepared next track, cancelling its download, and stops any preparation.
    pub(super) fn drop_prepared(&self, state: &mut State) {
        if let Some(preparing) = state.preparing.take() {
            preparing.cancel();
        }
        let Some(track_id) = state.prepared.as_ref().map(|(id, _)| *id) else {
            return;
        };
        let mut deck = self.deck();
        // once started, it is no longer next; its events are on the way and settle it
        if deck.next().is_some_and(|next| next.id == track_id) {
            deck.set_next(None);
            state.prepared = None;
            state.items.remove(&track_id);
            state.media.remove(&track_id);
        }
    }

    pub(super) fn started(&self, state: &mut State, id: TrackId) {
        state.current = Some(id);
        state.finished = None;
        let Some(item) = state.items.get(&id).cloned() else {
            return;
        };
        // the item leaves the queue first, so that clients see what comes next at `trackStart`
        if state
            .prepared
            .as_ref()
            .is_some_and(|(prepared, _)| *prepared == id)
        {
            state.prepared = None;
            let change = state.queue.advance(&item);
            self.report(&state.queue, change);
        }
        self.emit(Event::TrackStart {
            guild_id: self.guild_id,
            track: item.track,
            item_id: item.id,
        });
    }

    pub(super) fn ended(&self, state: &mut State, id: TrackId, reason: EndReason) {
        if state.current == Some(id) {
            state.current = None;
        }
        let reason = match reason {
            EndReason::Finished => TrackEndReason::Finished,
            EndReason::Stopped if state.destroying => TrackEndReason::Cleanup,
            EndReason::Stopped => TrackEndReason::Stopped,
            EndReason::Replaced => TrackEndReason::Replaced,
            EndReason::Failed => TrackEndReason::Failed,
        };
        let media = state.media.remove(&id);
        let Some(item) = state.items.remove(&id) else {
            return;
        };
        self.emit(Event::TrackEnd {
            guild_id: self.guild_id,
            track: item.track.clone(),
            item_id: item.id.clone(),
            reason,
        });
        if reason == TrackEndReason::Finished {
            // a repeated queue gets its tracks back at the end (a skip may have sent it there)
            if state.queue.repeat() == Repeat::Queue && state.queue.position(&item.id).is_none() {
                let change = state.queue.add(vec![item.clone()], None);
                self.report(&state.queue, change);
            }
        }
        // what it downloaded plays the next round, unless it failed
        if let Some(media) = media
            && matches!(reason, TrackEndReason::Finished | TrackEndReason::Replaced)
        {
            self.keep_for_next_round(state, &item, &media);
        }
        if reason == TrackEndReason::Finished {
            state.finished = Some(item);
        }
        if reason != TrackEndReason::Replaced {
            self.end_if_ran_out(state);
        }
    }

    /// Reports the end of the queue if nothing plays, nothing is on its way and nothing comes
    /// next (though not after a stop by the client).
    pub(super) fn end_if_ran_out(&self, state: &State) {
        let ran_out = !state.halted
            && !state.destroying
            && state.prepared.is_none()
            && state.preparing.is_none()
            && state.queue.upcoming(state.finished.as_ref()).is_none()
            && self.deck().is_idle();
        if ran_out {
            self.emit(Event::QueueEnd {
                guild_id: self.guild_id,
            });
        }
    }

    /// Tells clients about a queue change. Called with the state locked, so they see changes in
    /// the order of their versions.
    pub(super) fn report(&self, queue: &Queue, change: Option<QueueChange>) {
        if let Some(change) = change {
            self.emit(Event::QueueChanged {
                guild_id: self.guild_id,
                version: queue.version(),
                change,
            });
        }
    }
}
