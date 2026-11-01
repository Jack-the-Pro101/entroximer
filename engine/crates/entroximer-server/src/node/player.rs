//! A guild's player: a deck playing into a songbird voice connection, fed from a queue.
//!
//! Every deck event, whether the mixer thread or a request drained it, travels through one
//! channel to one task. A request waits for that task to catch up before it responds, so the
//! events it caused arrive before its response. Events are emitted with the player's state
//! locked, so clients see queue changes in the order of their versions.
//!
//! # Seamless queues
//! The item that comes next is prepared in the deck's gapless next slot: resolved and
//! downloading. A tick checks how much of the current track remains and starts preparing once
//! that drops below the preload lead, so the next track is buffered long before it is needed,
//! whatever the client does meanwhile. If the queue changes so that another item comes next, the
//! prepared one is dropped, which cancels its download. When the deck switches to the prepared
//! track, its item leaves the queue.
//!
//! A queue item that cannot be loaded is reported with `trackError` and removed, and the queue
//! goes on; after [`MAX_FAILURES`] in a row it waits for the client to change something.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use entroximer_core::deck::{Deck, DeckEvent, DeckTrack, EndReason, TrackId};
use entroximer_core::dsp::{GraphRunner, GraphSpec};
use entroximer_core::error::display_chain;
use entroximer_core::player::{PlaybackError, Player, SAMPLE_RATE};
use entroximer_core::timeline::{TICKS_PER_SECOND, Ticks, TimelineStats};
use songbird::events::context_data::DisconnectReason;
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::input::{Input, RawAdapter};
use songbird::tracks::TrackHandle;
use songbird::{ConnectionInfo, CoreEvent, Driver, EventContext};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::MissedTickBehavior;

use super::Node;
use super::output::DeckSource;
use super::queue::{Queue, QueueChange, QueueItem, Repeat};
use super::session::Session;
use super::tracks::Loaded;
use crate::api::protocol::{
    ApiError, ErrorCode, Event, Millis, PlayerInfo, PlayerState, PlayerUpdate, QueueAdded,
    QueuePage, QueueState, Snowflake, Track, TrackEndReason, VoiceState,
};

/// How often a player checks whether to prepare the next item.
const PRELOAD_TICK: Duration = Duration::from_millis(500);
/// Queue items failing to load in a row after which the queue waits for the client.
const MAX_FAILURES: u32 = 5;

/// Something for the player's event task.
pub(super) enum Signal {
    Deck(DeckEvent),
    /// Acknowledged once everything sent before it has been handled.
    Flush(oneshot::Sender<()>),
}

pub struct GuildPlayer {
    guild_id: Snowflake,
    user_id: Snowflake,
    me: Weak<GuildPlayer>,
    node: Weak<Node>,
    session: Weak<Session>,
    deck: Arc<Mutex<Deck>>,
    driver: tokio::sync::Mutex<Driver>,
    /// songbird's track playing the deck; paused while there is nothing to hear.
    output: TrackHandle,
    signals: mpsc::UnboundedSender<Signal>,
    state: Mutex<State>,
    next_track: AtomicU64,
    next_attempt: AtomicU64,
    /// Changes to one player apply one at a time, in the order they arrive.
    updating: tokio::sync::Mutex<()>,
}

/// Lock order: `state`, then `deck`; never the other way around.
#[derive(Default)]
struct State {
    /// The queue item each deck track plays.
    items: HashMap<TrackId, QueueItem>,
    current: Option<TrackId>,
    queue: Queue,
    /// The deck's next track, and the id of its item.
    prepared: Option<(TrackId, String)>,
    preparing: Option<Preparation>,
    /// The item that last played to its end, until another starts: repeating it works even if
    /// the deck went idle meanwhile.
    finished: Option<QueueItem>,
    /// The client stopped playback; the queue waits until it asks for something to play.
    halted: bool,
    /// Queue items that failed to load in a row.
    failures: u32,
    /// Paused by the client.
    paused: bool,
    /// The client's connection is down: playback waits for it to resume the session, so it
    /// misses nothing.
    client_away: bool,
    volume: u16,
    graph: Option<GraphSpec>,
    voice: Option<VoiceState>,
    connected: bool,
    destroying: bool,
}

/// An item being resolved in the background.
struct Preparation {
    item_id: String,
    /// Tells this preparation's outcome from a cancelled one's.
    attempt: u64,
    /// For a skip: the item replaces the current track as soon as it is ready, instead of
    /// following it.
    skip: bool,
    task: AbortHandle,
}

impl GuildPlayer {
    pub(super) fn spawn(
        node: &Arc<Node>,
        session: &Arc<Session>,
        guild_id: Snowflake,
    ) -> Arc<Self> {
        let deck = Arc::new(Mutex::new(Deck::new(node.settings.stuck_after)));
        let (signals, receiver) = mpsc::unbounded_channel();
        let player = Arc::new_cyclic(|me: &Weak<Self>| {
            let mut driver = Driver::new(songbird::Config::default());
            for event in [
                CoreEvent::DriverConnect,
                CoreEvent::DriverReconnect,
                CoreEvent::DriverDisconnect,
            ] {
                driver.add_global_event(event.into(), VoiceEvents(me.clone()));
            }
            let source = DeckSource::new(Arc::clone(&deck), signals.clone());
            let output =
                driver.play_only_input(Input::from(RawAdapter::new(source, SAMPLE_RATE, 2)));
            if let Err(e) = output.pause() {
                tracing::warn!(%guild_id, "cannot pause the new output: {e}");
            }
            Self {
                guild_id,
                user_id: session.user_id,
                me: me.clone(),
                node: Arc::downgrade(node),
                session: Arc::downgrade(session),
                deck,
                driver: tokio::sync::Mutex::new(driver),
                output,
                signals,
                state: Mutex::new(State {
                    volume: 100,
                    // a request may finish after its client disconnected
                    client_away: session.detached_epoch().is_some(),
                    ..State::default()
                }),
                next_track: AtomicU64::new(1),
                next_attempt: AtomicU64::new(1),
                updating: tokio::sync::Mutex::new(()),
            }
        });
        tokio::spawn(handle_signals(Arc::downgrade(&player), receiver));
        tokio::spawn(preload_ticks(Arc::downgrade(&player)));
        player
    }

    pub async fn update(&self, node: &Node, update: PlayerUpdate) -> Result<PlayerInfo, ApiError> {
        let _in_order = self.updating.lock().await;

        if let Some(voice) = update.voice {
            self.connect(voice).await?;
        }
        if let Some(volume) = update.volume {
            if volume > 1000 {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    "volume must be 0 to 1000",
                ));
            }
            self.deck().set_volume(f32::from(volume) / 100.0);
            self.state().volume = volume;
        }
        if let Some(graph) = update.graph {
            let runner = graph
                .as_ref()
                .map(|spec| node.registry.compile(spec, f64::from(SAMPLE_RATE)))
                .transpose()
                .map_err(|e| ApiError::new(ErrorCode::BadRequest, display_chain(&e)))?
                .map(|net| GraphRunner::new(Box::new(net)));
            self.deck().set_graph(runner);
            self.state().graph = graph;
        }
        if let Some(paused) = update.paused {
            self.state().paused = paused;
        }

        let end_at = update.end_time.map(|end| end.map(ticks));
        let mut seek = update.position.map(ticks);
        match update.track {
            Some(None) => self.halt(),
            Some(Some(selector)) => {
                let busy = !self.deck().is_idle();
                if !(busy && update.only_if_idle == Some(true)) {
                    let loaded = node.load(&selector).await?;
                    let item = QueueItem::new(loaded.track.clone());
                    // a position given with a track is where it starts
                    let track = self.deck_track(node, loaded, seek.take(), end_at.flatten());
                    let mut state = self.state();
                    state.halted = false;
                    // it also takes the place of an item a skip is waiting for
                    if let Some(skip) = state.preparing.take_if(|p| p.skip) {
                        skip.task.abort();
                    }
                    state.items.insert(track.id, item);
                    self.deck().play(track);
                }
            }
            None => {}
        }
        if let Some(end_at) = end_at {
            self.deck().set_end_at(end_at);
        }
        if let Some(position) = seek {
            self.deck().seek(position).map_err(|e| playback_error(&e))?;
        }

        self.settle().await;
        Ok(self.info())
    }

    pub fn queue_state(&self) -> QueueState {
        queue_state(&self.state().queue)
    }

    pub fn queue_page(&self, offset: usize, limit: usize) -> QueuePage {
        let state = self.state();
        QueuePage {
            queue: queue_state(&state.queue),
            offset: u32::try_from(offset).unwrap_or(u32::MAX),
            items: state.queue.page(offset, limit),
        }
    }

    /// Adds tracks to the queue at `index` (the end if `None` or past it). With nothing
    /// playing, the first item starts.
    pub async fn queue_add(&self, tracks: Vec<Track>, index: Option<u32>) -> QueueAdded {
        let _in_order = self.updating.lock().await;
        let items: Vec<QueueItem> = tracks.into_iter().map(QueueItem::new).collect();
        let added = {
            let mut state = self.state();
            let length = state.queue.len();
            let index = index.map_or(length, |i| i as usize).min(length);
            if !items.is_empty() {
                // asking for more to play ends a stop, and a wait after failures
                state.halted = false;
                state.failures = 0;
            }
            let change = state.queue.add(items.clone(), Some(index));
            self.report(&state.queue, change);
            self.plan_next(&mut state);
            QueueAdded {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                items,
                queue: queue_state(&state.queue),
            }
        };
        self.settle().await;
        added
    }

    pub async fn queue_remove(&self, ids: &[String]) -> Result<QueueState, ApiError> {
        self.edit_queue(|queue| Ok(queue.remove(ids))).await
    }

    pub async fn queue_move(&self, id: &str, index: u32) -> Result<QueueState, ApiError> {
        self.edit_queue(|queue| {
            queue.position(id).ok_or_else(|| no_item(id))?;
            Ok(queue.move_item(id, index as usize))
        })
        .await
    }

    pub async fn queue_clear(&self) -> Result<QueueState, ApiError> {
        self.edit_queue(|queue| Ok(queue.clear())).await
    }

    pub async fn queue_shuffle(&self) -> Result<QueueState, ApiError> {
        self.edit_queue(|queue| Ok(queue.shuffle(&mut fastrand::Rng::new())))
            .await
    }

    pub async fn set_repeat(&self, repeat: Repeat) -> Result<QueueState, ApiError> {
        self.edit_queue(|queue| Ok(queue.set_repeat(repeat))).await
    }

    /// Ends the current track and plays the first item of the queue (after dropping the items
    /// before `to`), or stops if the queue is empty. Instant when that item is prepared;
    /// otherwise the current track plays on until the item is ready. A track repeating is
    /// skipped too; with the whole queue repeating, the skipped item goes to its end.
    pub async fn skip(&self, to: Option<&str>) -> Result<PlayerInfo, ApiError> {
        let _in_order = self.updating.lock().await;
        // decide on what the deck is really playing
        self.drain_deck();
        self.flush().await;
        self.skip_locked(&mut self.state(), to)?;
        self.settle().await;
        Ok(self.info())
    }

    /// Stops playback and leaves the voice channel.
    pub async fn destroy(&self) {
        {
            let mut state = self.state();
            state.destroying = true;
            self.drop_prepared(&mut state);
        }
        self.deck().stop();
        self.drain_deck();
        self.flush().await;
        self.driver.lock().await.leave();
    }

    pub fn info(&self) -> PlayerInfo {
        let player_state = self.player_state();
        let state = self.state();
        let current = state.current.and_then(|id| state.items.get(&id));
        PlayerInfo {
            guild_id: self.guild_id,
            track: current.map(|item| item.track.clone()),
            item_id: current.map(|item| item.id.clone()),
            queue: queue_state(&state.queue),
            paused: state.paused,
            volume: state.volume,
            graph: state.graph.clone(),
            state: player_state,
        }
    }

    pub fn player_state(&self) -> PlayerState {
        let connected = self.state().connected;
        let deck = self.deck();
        let current = deck.current().map(|t| &t.player);
        PlayerState {
            time: Millis(unix_millis()),
            position: current.map_or(Millis(0), |p| millis(p.position())),
            connected,
            buffering: current.is_some_and(|p| p.buffering_since().is_some()),
            buffered: current.map_or_else(Vec::new, |p| {
                p.buffered()
                    .into_iter()
                    .map(|(start, end)| [millis(start), millis(end)])
                    .collect()
            }),
        }
    }

    /// Whether audio is flowing: something is loaded, the client has not paused it and is
    /// connected. (Without a voice connection songbird reads nothing either, so the position
    /// also holds while voice reconnects.)
    pub fn is_playing(&self) -> bool {
        let audible = {
            let state = self.state();
            !state.paused && !state.client_away
        };
        audible && !self.deck().is_idle()
    }

    /// Pauses playback while the client is away and continues it, from the same position, when
    /// the client resumes its session.
    pub fn set_client_present(&self, present: bool) {
        self.state().client_away = !present;
        self.sync_output();
    }

    /// Cached audio of the current and next tracks.
    pub fn cache_stats(&self) -> TimelineStats {
        let deck = self.deck();
        [deck.current(), deck.next()]
            .into_iter()
            .flatten()
            .map(|t| t.player.cache_stats())
            .fold(TimelineStats::default(), |sum, s| TimelineStats {
                ranges: sum.ranges + s.ranges,
                packets: sum.packets + s.packets,
                memory_bytes: sum.memory_bytes + s.memory_bytes,
                disk_bytes: sum.disk_bytes + s.disk_bytes,
            })
    }

    pub fn emit_update(&self) {
        self.emit(Event::PlayerUpdate {
            guild_id: self.guild_id,
            state: self.player_state(),
        });
    }

    /// Applies a client's edit to the queue and reports it.
    async fn edit_queue(
        &self,
        edit: impl FnOnce(&mut Queue) -> Result<Option<QueueChange>, ApiError>,
    ) -> Result<QueueState, ApiError> {
        let _in_order = self.updating.lock().await;
        let queue = {
            let mut state = self.state();
            let change = edit(&mut state.queue)?;
            if change.is_some() {
                // a queue waiting after failures gets going again
                state.failures = 0;
            }
            self.report(&state.queue, change);
            self.plan_next(&mut state);
            queue_state(&state.queue)
        };
        self.settle().await;
        Ok(queue)
    }

    fn skip_locked(&self, state: &mut State, to: Option<&str>) -> Result<(), ApiError> {
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
                pending.task.abort();
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
        Ok(())
    }

    /// Lets the deck's events reach clients and the output follow, before a response goes out.
    async fn settle(&self) {
        self.drain_deck();
        self.flush().await;
        self.sync_output();
    }

    /// The client stopped playback: the queue waits until it asks for something to play.
    fn halt(&self) {
        let mut state = self.state();
        state.halted = true;
        state.finished = None;
        self.drop_prepared(&mut state);
        self.deck().stop();
    }

    /// Prepares the item that comes next once the current track nears its end, and drops a
    /// prepared or preparing item that no longer comes next. Called with the state locked,
    /// right after it changed.
    fn plan_next(&self, state: &mut State) {
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

    /// Whether the current track ends within the preload lead, or nothing plays. A stream's end
    /// cannot be foreseen; what follows it is prepared once it ends.
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
            .map(ticks);
        let end = playing
            .end_at
            .or_else(|| playing.player.media_end())
            .or(length);
        let lead = ticks_from(node.settings.preload_lead);
        end.is_some_and(|end| end - playing.player.position() <= lead)
    }

    /// Resolves `item` in the background; see [`Self::finish_preparation`].
    fn start_preparation(&self, state: &mut State, item: QueueItem, skip: bool) {
        let Some(node) = self.node.upgrade() else {
            return;
        };
        if let Some(previous) = state.preparing.take() {
            previous.task.abort();
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
            task: task.abort_handle(),
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
                    state.failures = 0;
                    // resolving can tell more, e.g. a YouTube Music result's artist and length
                    let track = Track {
                        user_data: item.track.user_data.take(),
                        ..loaded.track.clone()
                    };
                    let change = state.queue.update(&item.id, track.clone());
                    self.report(&state.queue, change);
                    item.track = track;
                    let deck_track = self.deck_track(node, loaded, None, None);
                    state.prepared = Some((deck_track.id, item.id.clone()));
                    state.items.insert(deck_track.id, item);
                    let mut deck = self.deck();
                    if preparation.skip {
                        deck.play(deck_track);
                    } else {
                        deck.set_next(Some(deck_track));
                    }
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

    /// The item a skip waited for failed: skip to the next one, or stop.
    fn continue_skip(&self, state: &mut State) {
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
    fn drop_prepared(&self, state: &mut State) {
        if let Some(preparing) = state.preparing.take() {
            preparing.task.abort();
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
        }
    }

    fn deck_track(
        &self,
        node: &Node,
        loaded: Loaded,
        start: Option<Ticks>,
        end_at: Option<Ticks>,
    ) -> DeckTrack {
        DeckTrack {
            id: self.next_track.fetch_add(1, Ordering::Relaxed),
            player: Player::new(
                Handle::current(),
                loaded.producer,
                node.ctx.player_config(),
                start,
            ),
            end_at,
        }
    }

    async fn connect(&self, voice: VoiceState) -> Result<(), ApiError> {
        if self.state().voice.as_ref() == Some(&voice) {
            return Ok(());
        }
        let id = |snowflake: Snowflake, what: &str| {
            NonZeroU64::new(snowflake.0).ok_or_else(|| {
                ApiError::new(ErrorCode::BadRequest, format!("{what} must not be 0"))
            })
        };
        let info = ConnectionInfo {
            channel_id: ChannelId(id(voice.channel_id, "channelId")?),
            endpoint: voice.endpoint.clone(),
            guild_id: GuildId(id(self.guild_id, "guildId")?),
            session_id: voice.session_id.clone(),
            token: voice.token.clone(),
            user_id: UserId(id(self.user_id, "the bot's user ID")?),
        };
        {
            let mut state = self.state();
            state.voice = Some(voice);
            state.connected = false;
        }
        let connecting = self.driver.lock().await.connect(info);
        // Discord answers asynchronously; the outcome arrives as an event, not in the response
        let me = self.me.clone();
        tokio::spawn(async move {
            let result = connecting.await;
            let Some(player) = me.upgrade() else { return };
            match result {
                Ok(()) => player.state().connected = true,
                Err(e) => player.emit(Event::VoiceClosed {
                    guild_id: player.guild_id,
                    code: None,
                    reason: format!("could not connect: {e}"),
                    by_remote: false,
                }),
            }
        });
        Ok(())
    }

    /// Passes events the deck produced during a request to the event task.
    fn drain_deck(&self) {
        let events = self.deck().drain_events();
        for event in events {
            let _ = self.signals.send(Signal::Deck(event));
        }
    }

    /// Waits until the event task has handled everything sent so far.
    async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.signals.send(Signal::Flush(done)).is_ok() {
            let _ = flushed.await;
        }
    }

    fn handle(&self, event: DeckEvent) {
        {
            let mut state = self.state();
            match event {
                DeckEvent::Started(id) => self.started(&mut state, id),
                DeckEvent::Ended(id, reason) => self.ended(&mut state, id, reason),
                DeckEvent::Failed(id, error) => {
                    if let Some(item) = state.items.get(&id) {
                        self.emit(Event::TrackError {
                            guild_id: self.guild_id,
                            track: item.track.clone(),
                            item_id: item.id.clone(),
                            error: playback_error(&error),
                        });
                    }
                }
                DeckEvent::Stuck(id, waiting) => {
                    if let Some(item) = state.items.get(&id) {
                        self.emit(Event::TrackStuck {
                            guild_id: self.guild_id,
                            track: item.track.clone(),
                            item_id: item.id.clone(),
                            waiting: Millis(u64::try_from(waiting.as_millis()).unwrap_or(u64::MAX)),
                        });
                    }
                }
            }
            self.plan_next(&mut state);
        }
        self.sync_output();
    }

    fn started(&self, state: &mut State, id: TrackId) {
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

    fn ended(&self, state: &mut State, id: TrackId, reason: EndReason) {
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
            state.finished = Some(item);
        }
        if reason != TrackEndReason::Replaced {
            self.end_if_ran_out(state);
        }
    }

    /// Reports the end of the queue if nothing plays, nothing is on its way and nothing comes
    /// next (though not after a stop by the client).
    fn end_if_ran_out(&self, state: &State) {
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
    fn report(&self, queue: &Queue, change: Option<QueueChange>) {
        if let Some(change) = change {
            self.emit(Event::QueueChanged {
                guild_id: self.guild_id,
                version: queue.version(),
                change,
            });
        }
    }

    /// Plays the songbird output only while there is something to hear, so Discord gets no
    /// stream of silence.
    fn sync_output(&self) {
        let result = if self.is_playing() {
            self.output.play()
        } else {
            self.output.pause()
        };
        if let Err(e) = result {
            tracing::warn!(guild_id = %self.guild_id, "cannot control the voice output: {e}");
        }
    }

    fn emit(&self, event: Event) {
        if let Some(session) = self.session.upgrade() {
            session.emit(event);
        }
    }

    fn deck(&self) -> MutexGuard<'_, Deck> {
        self.deck.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for GuildPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuildPlayer")
            .field("guild_id", &self.guild_id)
            .finish_non_exhaustive()
    }
}

fn queue_state(queue: &Queue) -> QueueState {
    QueueState {
        version: queue.version(),
        length: u32::try_from(queue.len()).unwrap_or(u32::MAX),
        repeat: queue.repeat(),
    }
}

fn no_item(id: &str) -> ApiError {
    ApiError::new(
        ErrorCode::NotFound,
        format!("there is no item `{id}` in the queue"),
    )
}

async fn handle_signals(player: Weak<GuildPlayer>, mut signals: mpsc::UnboundedReceiver<Signal>) {
    while let Some(signal) = signals.recv().await {
        match signal {
            Signal::Deck(event) => match player.upgrade() {
                Some(player) => player.handle(event),
                None => break,
            },
            Signal::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Checks regularly whether to prepare the next item, while the player exists.
async fn preload_ticks(player: Weak<GuildPlayer>) {
    let mut ticks = tokio::time::interval(PRELOAD_TICK);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let Some(player) = player.upgrade() else {
            return;
        };
        let mut state = player.state();
        if state.destroying {
            return;
        }
        player.plan_next(&mut state);
    }
}

/// Reports the voice connection's state.
struct VoiceEvents(Weak<GuildPlayer>);

#[async_trait]
impl songbird::EventHandler for VoiceEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<songbird::Event> {
        let player = self.0.upgrade()?;
        match ctx {
            EventContext::DriverConnect(_) | EventContext::DriverReconnect(_) => {
                player.state().connected = true;
                player.sync_output();
            }
            EventContext::DriverDisconnect(data) => {
                player.state().connected = false;
                let (code, by_remote) = match data.reason {
                    Some(DisconnectReason::WsClosed(code)) => (code.map(|c| c as u16), true),
                    _ => (None, false),
                };
                player.emit(Event::VoiceClosed {
                    guild_id: player.guild_id,
                    code,
                    reason: data
                        .reason
                        .map_or_else(|| "disconnected".to_owned(), |r| format!("{r:?}")),
                    by_remote,
                });
            }
            _ => {}
        }
        None
    }
}

fn playback_error(e: &PlaybackError) -> ApiError {
    let code = match e {
        PlaybackError::Media(_) => ErrorCode::LoadFailed,
        PlaybackError::Decode(_) | PlaybackError::Resample(_) | PlaybackError::Cache(_) => {
            ErrorCode::PlaybackFailed
        }
    };
    ApiError::new(code, display_chain(e))
}

fn ticks(ms: Millis) -> Ticks {
    i64::try_from(ms.0)
        .unwrap_or(i64::MAX)
        .saturating_mul(TICKS_PER_SECOND / 1000)
}

fn ticks_from(duration: Duration) -> Ticks {
    ticks(Millis(
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
    ))
}

fn millis(t: Ticks) -> Millis {
    Millis(u64::try_from(t / (TICKS_PER_SECOND / 1000)).unwrap_or(0))
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
