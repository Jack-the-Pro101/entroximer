//! A guild's player: a mixer playing into a songbird voice connection, its music layer's deck
//! fed from a queue.
//!
//! Every deck event, whether the mixer thread or a request drained it, travels through one
//! channel to one task. A request waits for that task to catch up before it responds, so the
//! events it caused arrive before its response. Events are emitted with the player's state
//! locked, so clients see queue changes in the order of their versions.
//!
//! The queue's scheduling is in [`queue_flow`], the other layers and their clips in [`layers`],
//! the voice connection in [`connection`].

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use entroximer_core::deck::{Deck, DeckEvent, DeckTrack, EndReason, TrackId};
use entroximer_core::dsp::{Effects, GraphSpec, PresetSelection};
use entroximer_core::error::display_chain;
use entroximer_core::mixer::Mixer;
use entroximer_core::player::{PlaybackError, Player, SAMPLE_RATE};
use entroximer_core::timeline::{Ticks, TimelineStats, ticks};
use songbird::events::context_data::DisconnectReason;
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::input::{Input, RawAdapter};
use songbird::tracks::TrackHandle;
use songbird::{ConnectionInfo, CoreEvent, Driver, EventContext};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::MissedTickBehavior;

pub use layers::ClipTurn;

use super::Node;
use super::output::MixerSource;
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
/// Percent.
const MAX_VOLUME: u16 = 1000;
/// Items one queue may hold.
const MAX_QUEUE_LENGTH: usize = 10_000;
/// The longest crossfade between queue tracks.
const MAX_CROSSFADE: Millis = Millis(12_000);

/// Something for the player's event task.
pub(super) enum Signal {
    Deck(DeckEvent),
    /// Acknowledged once everything sent before it has been handled.
    Flush(oneshot::Sender<()>),
}

pub struct GuildPlayer {
    guild_id: Snowflake,
    user_id: Snowflake,
    me: Weak<Self>,
    node: Weak<Node>,
    session: Weak<Session>,
    mixer: Arc<Mutex<Mixer>>,
    driver: tokio::sync::Mutex<Driver>,
    /// songbird's track playing the mixer; paused while there is nothing to hear.
    output: TrackHandle,
    /// Whether `output` was last told to play; held while telling it, so that changes apply in
    /// order. `None` once telling it failed.
    output_playing: Mutex<Option<bool>>,
    signals: mpsc::UnboundedSender<Signal>,
    state: Mutex<State>,
    next_track: AtomicU64,
    next_attempt: AtomicU64,
    /// Changes to one player apply one at a time, in the order they arrive.
    updating: tokio::sync::Mutex<()>,
}

/// Lock order: `output_playing`, `state`, then `mixer`; never the other way around.
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
    /// The layers besides the music, in the order they were made.
    layers: Vec<layers::LayerState>,
    /// The clips in the layers' decks, by deck track, in the order they were played.
    clips: BTreeMap<TrackId, layers::ClipEntry>,
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
        let mixer = Arc::new(Mutex::new(Mixer::new(node.settings.stuck_after)));
        let (signals, receiver) = mpsc::unbounded_channel();
        let player = Arc::new_cyclic(|me: &Weak<Self>| {
            let mut driver = Driver::new(songbird::Config::default());
            for event in [
                CoreEvent::DriverConnect,
                CoreEvent::DriverReconnect,
                CoreEvent::DriverDisconnect,
            ] {
                driver.add_global_event(event.into(), connection::VoiceEvents(me.clone()));
            }
            let source = MixerSource::new(Arc::clone(&mixer), signals.clone());
            let output =
                driver.play_only_input(Input::from(RawAdapter::new(source, SAMPLE_RATE, 2)));
            let paused = output.pause();
            if let Err(e) = &paused {
                tracing::warn!(%guild_id, "cannot pause the new output: {e}");
            }
            Self {
                guild_id,
                user_id: session.user_id,
                me: me.clone(),
                node: Arc::downgrade(node),
                session: Arc::downgrade(session),
                mixer,
                driver: tokio::sync::Mutex::new(driver),
                output,
                output_playing: Mutex::new(paused.is_ok().then_some(false)),
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

    /// Applies a client's changes. Everything that can be refused is checked before anything
    /// changes, so a refused update changes nothing (except joining voice, which stands on its
    /// own).
    pub async fn update(&self, node: &Node, update: PlayerUpdate) -> Result<PlayerInfo, ApiError> {
        let _in_order = self.updating.lock().await;

        if update.volume.is_some_and(|volume| volume > MAX_VOLUME) {
            return Err(bad_request(format!("volume must be 0 to {MAX_VOLUME}")));
        }
        let effects = compile_effects(node, update.graph, update.preset)?;
        let voice = update
            .voice
            .map(|voice| self.connection_info(&voice).map(|info| (voice, info)))
            .transpose()?;
        if let Some((voice, info)) = voice {
            self.connect(voice, info).await;
        }
        // `None`: leave the track alone; `Some(None)`: stop
        let track = match update.track {
            Some(Some(_)) if update.only_if_idle == Some(true) && !self.deck().is_idle() => None,
            Some(Some(selector)) => Some(Some(node.load(&selector).await?)),
            Some(None) => Some(None),
            None => None,
        };

        if let Some(volume) = update.volume {
            self.deck().set_volume(f32::from(volume) / 100.0);
            self.state().volume = volume;
        }
        if let Some((graph, effects)) = effects {
            self.deck().set_effects(effects);
            self.state().graph = graph;
        }
        if let Some(paused) = update.paused {
            self.state().paused = paused;
            self.mixer().main_mut().paused = paused;
        }
        let end_at = update.end_time.map(|end| end.map(Millis::to_ticks));
        let mut seek = update.position.map(Millis::to_ticks);
        match track {
            Some(None) => self.halt(),
            Some(Some(loaded)) => {
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
    pub async fn queue_add(
        &self,
        tracks: Vec<Track>,
        index: Option<u32>,
    ) -> Result<QueueAdded, ApiError> {
        let _in_order = self.updating.lock().await;
        let items: Vec<QueueItem> = tracks.into_iter().map(QueueItem::new).collect();
        let added = {
            let mut state = self.state();
            let length = state.queue.len();
            if length + items.len() > MAX_QUEUE_LENGTH {
                return Err(bad_request(format!(
                    "a queue holds at most {MAX_QUEUE_LENGTH} items; it has {length}"
                )));
            }
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
        Ok(added)
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

    /// Sets what repeats and how long tracks crossfade; a crossfade over 12 s is refused, and
    /// changes nothing.
    pub async fn configure_queue(
        &self,
        repeat: Option<Repeat>,
        crossfade: Option<Millis>,
    ) -> Result<QueueState, ApiError> {
        if crossfade.is_some_and(|crossfade| crossfade > MAX_CROSSFADE) {
            return Err(bad_request(format!(
                "crossfade must be 0 to {} ms",
                MAX_CROSSFADE.0
            )));
        }
        self.edit_queue(|queue| {
            if let Some(crossfade) = crossfade {
                queue.set_crossfade(crossfade);
                self.deck().set_crossfade(crossfade.to_ticks());
            }
            Ok(repeat.and_then(|repeat| queue.set_repeat(repeat)))
        })
        .await
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
            self.end_clips(&mut state);
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
            layers: self.layer_infos(&state),
        }
    }

    pub fn player_state(&self) -> PlayerState {
        let connected = self.state().connected;
        let deck = self.deck();
        let current = deck.current().map(|t| &t.player);
        PlayerState {
            time: Millis::now(),
            position: current.map_or(Millis(0), |p| Millis::from_ticks(p.position())),
            connected,
            buffering: current.is_some_and(|p| p.buffering_since().is_some()),
            buffered: current.map_or_else(Vec::new, |p| {
                p.buffered()
                    .into_iter()
                    .map(|(start, end)| [Millis::from_ticks(start), Millis::from_ticks(end)])
                    .collect()
            }),
        }
    }

    /// Whether audio is flowing: some layer that the client has not paused has something to
    /// play, and the client is connected. (Without a voice connection songbird reads nothing
    /// either, so positions also hold while voice reconnects.)
    pub fn is_playing(&self) -> bool {
        let present = !self.state().client_away;
        present && self.mixer().is_audible()
    }

    /// Pauses playback while the client is away and continues it, from the same position, when
    /// the client resumes its session.
    pub fn set_client_present(&self, present: bool) {
        self.state().client_away = !present;
        self.sync_output();
    }

    /// Cached audio of every track the player holds.
    pub fn cache_stats(&self) -> TimelineStats {
        self.mixer()
            .players()
            .map(entroximer_core::player::Player::cache_stats)
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

    /// Lets the deck's events reach clients and the output follow, before a response goes out.
    async fn settle(&self) {
        self.drain_deck();
        self.flush().await;
        self.sync_output();
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
            length: loaded.track.info.length.map(Millis::to_ticks),
            player: Player::new(
                Handle::current(),
                loaded.producer,
                node.ctx.player_config(),
                start,
            ),
            end_at,
        }
    }

    /// Passes events the mixer produced during a request to the event task.
    fn drain_deck(&self) {
        let events = self.mixer().drain_events();
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
            let id = match &event {
                DeckEvent::Started(id)
                | DeckEvent::Ended(id, _)
                | DeckEvent::Failed(id, _)
                | DeckEvent::Stuck(id, _) => *id,
            };
            if layers::is_clip(id) {
                self.clip_event(&mut state, id, event);
                drop(state);
                self.sync_output();
                return;
            }
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
                            waiting: Millis::from(waiting),
                        });
                    }
                }
            }
            self.plan_next(&mut state);
        }
        self.sync_output();
    }

    /// Plays the songbird output only while there is something to hear, so Discord gets no
    /// stream of silence. Tells songbird only about changes: this runs after every event.
    fn sync_output(&self) {
        let mut output_playing = self
            .output_playing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let playing = self.is_playing();
        if *output_playing == Some(playing) {
            return;
        }
        let result = if playing {
            self.output.play()
        } else {
            self.output.pause()
        };
        *output_playing = match result {
            Ok(()) => Some(playing),
            Err(e) => {
                tracing::warn!(guild_id = %self.guild_id, "cannot control the voice output: {e}");
                None
            }
        };
    }

    fn emit(&self, event: Event) {
        if let Some(session) = self.session.upgrade() {
            session.emit(event);
        }
    }

    /// The music's deck, which the queue plays into.
    fn deck(&self) -> DeckGuard<'_> {
        DeckGuard(self.mixer())
    }

    fn mixer(&self) -> MutexGuard<'_, Mixer> {
        self.mixer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The mixer, locked, as its music deck.
struct DeckGuard<'a>(MutexGuard<'a, Mixer>);

impl Deref for DeckGuard<'_> {
    type Target = Deck;

    fn deref(&self) -> &Deck {
        &self.0.main().deck
    }
}

impl DerefMut for DeckGuard<'_> {
    fn deref_mut(&mut self) -> &mut Deck {
        &mut self.0.main_mut().deck
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
        crossfade: queue.crossfade(),
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

/// Checks regularly whether to prepare the next item, while the player exists, and whether the
/// output still has something to play: tails of cut-off tracks end without an event.
async fn preload_ticks(player: Weak<GuildPlayer>) {
    let mut ticks = tokio::time::interval(PRELOAD_TICK);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let Some(player) = player.upgrade() else {
            return;
        };
        {
            let mut state = player.state();
            if state.destroying {
                return;
            }
            player.plan_next(&mut state);
        }
        player.sync_output();
    }
}

/// New effects: the graph to show, and what it compiles to; `None` for none.
type NewEffects = (Option<GraphSpec>, Option<Effects>);

/// Checks a client's new effects, given as a graph or a preset; `None` leaves them alone.
fn compile_effects(
    node: &Node,
    graph: Option<Option<GraphSpec>>,
    preset: Option<PresetSelection>,
) -> Result<Option<NewEffects>, ApiError> {
    let graph = match (graph, preset) {
        (Some(_), Some(_)) => {
            return Err(bad_request("give either `graph` or `preset`, not both"));
        }
        (Some(graph), None) => graph,
        (None, Some(preset)) => Some(
            node.presets
                .build(&preset)
                .map_err(|e| bad_request(display_chain(&e)))?,
        ),
        (None, None) => return Ok(None),
    };
    let effects = graph
        .as_ref()
        .map(|spec| node.registry.compile(spec, f64::from(SAMPLE_RATE)))
        .transpose()
        .map_err(|e| bad_request(display_chain(&e)))?;
    Ok(Some((graph, effects)))
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

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::BadRequest, message)
}

mod connection;
mod layers;
mod queue_flow;
#[cfg(test)]
mod tests;
