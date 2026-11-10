//! Layers besides the music, and the clips they play: sounds outside the queue, such as
//! announcements or sound effects, each layer with its own effects, volume and pause.
//!
//! A clip loads before it touches the player, without its lock, so a slow one holds up nothing;
//! then it joins its layer in the order its request arrived (see [`ClipTurn`]). On a `queue`
//! layer only the playing clip and the next one have a [`Player`], downloading; the others wait
//! as loaded tracks. Every clip accepted ends once: the deck reports those that started, and
//! those that never did are reported here.

use std::collections::VecDeque;

use entroximer_core::deck::{Clip, ClipMode as DeckMode, Placed};
use entroximer_core::mixer::{self, MAIN};
use entroximer_core::player::PlayerConfig;

use super::*;
use crate::api::protocol::{
    ClipInfo, ClipMode, ClipStatus, Duck, LayerInfo, LayerUpdate, PlayClip, StopClips,
};

/// Layers a player may have besides the music.
const MAX_LAYERS: usize = 4;
/// Clips a `mix` layer plays at once, at most.
const MAX_MIXED: u8 = 8;
/// Clips waiting their turn on one layer.
const MAX_WAITING: usize = 32;
const MAX_FADE: Millis = Millis(10_000);
const MAX_CLIP_ID: usize = 64;
const MAX_LAYER_NAME: usize = 32;
/// Set in the deck ids of clips, so that their events never reach the queue's, even once the
/// clip is forgotten.
const CLIP_ID: TrackId = 1 << 63;
/// A clip that takes longer to load fails.
const CLIP_LOAD_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_ATTACK: Millis = Millis(50);
const DEFAULT_RELEASE: Millis = Millis(500);
const DEFAULT_HOLD: Millis = Millis(250);

/// A layer's settings, as its client set them.
pub(super) struct LayerState {
    name: String,
    mode: ClipMode,
    max_clips: u8,
    volume: u16,
    clipping: bool,
    paused: bool,
    graph: Option<GraphSpec>,
    duck: Option<Duck>,
    /// Clips waiting their turn on a `queue` layer, loaded but not opened yet.
    waiting: VecDeque<Waiting>,
}

impl LayerState {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            mode: ClipMode::Queue,
            max_clips: MAX_MIXED,
            volume: 100,
            clipping: false,
            paused: false,
            graph: None,
            duck: None,
            waiting: VecDeque::new(),
        }
    }

    fn deck_mode(&self) -> DeckMode {
        match self.mode {
            ClipMode::Queue => DeckMode::Queue,
            ClipMode::Mix => DeckMode::Mix(usize::from(self.max_clips)),
            ClipMode::Replace => DeckMode::Replace,
        }
    }
}

/// A clip in a layer's deck: playing, or next on a `queue` layer.
pub(super) struct ClipEntry {
    clip_id: String,
    layer: String,
    track: Track,
    /// Whether its `clipStart` went out; until then, it says it is waiting.
    started: bool,
}

/// Whether a deck track is a clip, rather than a track of the queue.
pub(super) fn is_clip(id: TrackId) -> bool {
    id & CLIP_ID != 0
}

/// A clip waiting for room on its layer.
struct Waiting {
    clip_id: String,
    loaded: Loaded,
    options: ClipOptions,
}

/// How a clip plays, in the deck's terms.
#[derive(Clone, Copy, Default)]
pub(super) struct ClipOptions {
    volume: f32,
    position: Option<Ticks>,
    end_at: Option<Ticks>,
    fade_in: usize,
    fade_out: usize,
    looping: bool,
}

/// A `clip.play`'s place among those for the same layer, taken as its request arrived: it joins
/// the layer once every earlier one has, or has failed. Dropping it lets the next one go.
#[derive(Debug)]
pub struct ClipTurn {
    earlier: Option<oneshot::Receiver<()>>,
    _done: oneshot::Sender<()>,
}

impl ClipTurn {
    /// A turn after the one `earlier` belongs to; the receiver tells the turn after this one.
    pub(crate) fn after(earlier: Option<oneshot::Receiver<()>>) -> (Self, oneshot::Receiver<()>) {
        let (done, finished) = oneshot::channel();
        let turn = Self {
            earlier,
            _done: done,
        };
        (turn, finished)
    }

    async fn wait(&mut self) {
        if let Some(earlier) = self.earlier.take() {
            // an error means the earlier turn is over all the same
            let _ = earlier.await;
        }
    }
}

impl GuildPlayer {
    /// Creates a layer if needed and changes it. Everything that can be refused is checked
    /// before anything changes.
    pub async fn layer_update(
        &self,
        node: &Node,
        update: LayerUpdate,
    ) -> Result<LayerInfo, ApiError> {
        let _in_order = self.updating.lock().await;
        let name = update.layer.as_str();
        check_layer_name(name)?;
        if update.volume.is_some_and(|volume| volume > MAX_VOLUME) {
            return Err(bad_request(format!("volume must be 0 to {MAX_VOLUME}")));
        }
        if update
            .max_clips
            .is_some_and(|most| !(1..=MAX_MIXED).contains(&most))
        {
            return Err(bad_request(format!("maxClips must be 1 to {MAX_MIXED}")));
        }
        let effects = compile_effects(node, update.graph, update.preset)?;
        // the effects it replaces are freed once the mixer is unlocked again
        let replaced_effects = {
            let mut state = self.state();
            let exists = state.layers.iter().any(|layer| layer.name == name);
            if !exists {
                check_room_for_layer(&state)?;
            }
            if let Some(Some(duck)) = &update.duck {
                check_duck(&state, name, duck)?;
            }
            if let Some(mode) = update.mode
                && exists
                && mode != layer(&state, name).mode
                && has_clips(&state, name)
            {
                return Err(bad_request(
                    "a layer's mode changes only while it plays no clips",
                ));
            }

            let mut mixer = self.mixer();
            let deck_layer = mixer.layer_or_add(name);
            if !exists {
                state.layers.push(LayerState::new(name));
            }
            let layer = layer_mut(&mut state, name);
            if let Some(volume) = update.volume {
                layer.volume = volume;
                deck_layer.deck.set_volume(f32::from(volume) / 100.0);
            }
            if let Some(clipping) = update.clipping {
                layer.clipping = clipping;
                deck_layer.deck.set_clipping(clipping);
            }
            let replaced_effects = effects.and_then(|(graph, effects)| {
                layer.graph = graph;
                deck_layer.deck.set_effects(effects)
            });
            if let Some(paused) = update.paused {
                layer.paused = paused;
                deck_layer.paused = paused;
            }
            if let Some(mode) = update.mode {
                layer.mode = mode;
            }
            if let Some(most) = update.max_clips {
                layer.max_clips = most;
            }
            if let Some(duck) = update.duck {
                // as it is in effect, with the defaults
                let duck = duck.map(|duck| Duck {
                    attack: Some(duck.attack.unwrap_or(DEFAULT_ATTACK)),
                    release: Some(duck.release.unwrap_or(DEFAULT_RELEASE)),
                    hold: Some(duck.hold.unwrap_or(DEFAULT_HOLD)),
                    ..duck
                });
                deck_layer.duck = duck.as_ref().map(deck_duck);
                layer.duck = duck;
            }
            replaced_effects
        };
        drop(replaced_effects);
        self.settle().await;
        Ok(self.layer_info(name))
    }

    /// Removes a layer, ending its clips (as stopped); other layers stop ducking it.
    pub async fn layer_remove(&self, name: &str) -> Result<(), ApiError> {
        let _in_order = self.updating.lock().await;
        check_layer_name(name)?;
        {
            let mut state = self.state();
            if !state.layers.iter().any(|layer| layer.name == name) {
                return Err(no_layer(name));
            }
            self.drop_unstarted(&mut state, name, TrackEndReason::Stopped);
            state.layers.retain(|layer| layer.name != name);
            let mut mixer = self.mixer();
            mixer.remove_layer(name);
            for layer in &mut state.layers {
                if let Some(duck) = &mut layer.duck {
                    duck.layers.retain(|target| target != name);
                    if let Some(deck_layer) = mixer.layer_mut(&layer.name) {
                        deck_layer.duck = Some(deck_duck(duck));
                    }
                }
            }
        }
        self.settle().await;
        Ok(())
    }

    /// Loads a clip and plays it on its layer, creating the layer if needed; it waits for the
    /// clips sent before it for the same layer (`turn`) before it joins.
    pub async fn clip_play(
        &self,
        node: &Node,
        clip: PlayClip,
        mut turn: Option<ClipTurn>,
    ) -> Result<ClipInfo, ApiError> {
        let name = clip.layer.as_str();
        check_layer_name(name)?;
        if clip.volume.is_some_and(|volume| volume > MAX_VOLUME) {
            return Err(bad_request(format!("volume must be 0 to {MAX_VOLUME}")));
        }
        if [clip.fade_in, clip.fade_out]
            .into_iter()
            .flatten()
            .any(|fade| fade > MAX_FADE)
        {
            return Err(bad_request(format!("fades take at most {} ms", MAX_FADE.0)));
        }
        if clip
            .clip_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.chars().count() > MAX_CLIP_ID)
        {
            return Err(bad_request(format!(
                "clipId must be 1 to {MAX_CLIP_ID} characters"
            )));
        }
        // in place of its track's times
        let times = clip.track.times();
        let options = ClipOptions {
            volume: f32::from(clip.volume.unwrap_or(100)) / 100.0,
            position: clip.position.or(times.start).map(Millis::to_ticks),
            end_at: clip.end_time.or(times.end).map(Millis::to_ticks),
            fade_in: frames(clip.fade_in.unwrap_or_default()),
            fade_out: frames(clip.fade_out.unwrap_or_default()),
            looping: clip.looping == Some(true),
        };

        let loaded = {
            // a burst of clips must not start a burst of downloads
            let _loading =
                node.clip_loads.acquire().await.map_err(|_| {
                    ApiError::new(ErrorCode::Internal, "the engine is shutting down")
                })?;
            tokio::time::timeout(CLIP_LOAD_TIMEOUT, node.load(&clip.track))
                .await
                .map_err(|_| {
                    ApiError::new(
                        ErrorCode::LoadFailed,
                        format!(
                            "loading the clip took over {} s",
                            CLIP_LOAD_TIMEOUT.as_secs()
                        ),
                    )
                })??
        };
        if let Some(turn) = &mut turn {
            turn.wait().await;
        }
        self.add_clip(node, name, clip.clip_id, loaded, options)
            .await
    }

    /// Puts a loaded clip on its layer, creating the layer if needed.
    async fn add_clip(
        &self,
        node: &Node,
        name: &str,
        clip_id: Option<String>,
        loaded: Loaded,
        options: ClipOptions,
    ) -> Result<ClipInfo, ApiError> {
        let _in_order = self.updating.lock().await;
        let info = {
            let mut state = self.state();
            if state.destroying {
                return Err(ApiError::new(
                    ErrorCode::NotFound,
                    "the player was destroyed meanwhile",
                ));
            }
            let exists = state.layers.iter().any(|layer| layer.name == name);
            if !exists {
                check_room_for_layer(&state)?;
            }
            let clip_id = clip_id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            if clip_in_use(&state, &clip_id) {
                return Err(bad_request(format!("clip `{clip_id}` is playing already")));
            }
            let mode = if exists {
                layer(&state, name).mode
            } else {
                ClipMode::Queue
            };
            if options.looping && mode == ClipMode::Queue {
                return Err(bad_request(
                    "clips loop only on `mix` and `replace` layers: a loop would hold up the \
                     clips after it",
                ));
            }
            let has_room = {
                let mixer = self.mixer();
                let deck_mode = if exists {
                    layer(&state, name).deck_mode()
                } else {
                    DeckMode::Queue
                };
                mixer
                    .layer(name)
                    .is_none_or(|layer| layer.deck.has_room(deck_mode))
            };
            if !has_room && layer(&state, name).waiting.len() >= MAX_WAITING {
                return Err(bad_request(format!(
                    "a layer holds at most {MAX_WAITING} waiting clips"
                )));
            }

            if !exists {
                state.layers.push(LayerState::new(name));
                self.mixer().layer_or_add(name);
            }
            let track = loaded.track.clone();
            let status = if has_room {
                if mode == ClipMode::Replace {
                    self.drop_unstarted(&mut state, name, TrackEndReason::Replaced);
                }
                match self.place(node, &mut state, name, clip_id.clone(), loaded, options) {
                    Placed::Playing => ClipStatus::Playing,
                    Placed::Next => ClipStatus::Waiting,
                }
            } else {
                layer_mut(&mut state, name).waiting.push_back(Waiting {
                    clip_id: clip_id.clone(),
                    loaded,
                    options,
                });
                ClipStatus::Waiting
            };
            ClipInfo {
                clip_id,
                layer: name.to_owned(),
                track,
                status,
                position: Millis::from_ticks(options.position.unwrap_or(0)),
            }
        };
        self.settle().await;
        Ok(info)
    }

    /// Stops a clip of a layer, or every clip of it, waiting ones too.
    pub async fn clip_stop(&self, stop: StopClips) -> Result<LayerInfo, ApiError> {
        let _in_order = self.updating.lock().await;
        let name = stop.layer.as_str();
        check_layer_name(name)?;
        if stop.fade_out.is_some_and(|fade| fade > MAX_FADE) {
            return Err(bad_request(format!("fades take at most {} ms", MAX_FADE.0)));
        }
        let fade = stop.fade_out.map(frames);
        {
            let mut state = self.state();
            if !state.layers.iter().any(|layer| layer.name == name) {
                return Err(no_layer(name));
            }
            match stop.clip_id {
                None => {
                    self.drop_unstarted(&mut state, name, TrackEndReason::Stopped);
                    if let Some(layer) = self.mixer().layer_mut(name) {
                        layer.deck.end_all(EndReason::Stopped, fade);
                    }
                }
                Some(clip_id) => self.stop_clip(&mut state, name, &clip_id, fade)?,
            }
        }
        self.settle().await;
        Ok(self.layer_info(name))
    }

    fn stop_clip(
        &self,
        state: &mut State,
        name: &str,
        clip_id: &str,
        fade: Option<usize>,
    ) -> Result<(), ApiError> {
        let waiting = &mut layer_mut(state, name).waiting;
        if let Some(index) = waiting.iter().position(|w| w.clip_id == clip_id) {
            let waiting = waiting.remove(index).expect("found above");
            self.emit_end(
                name,
                waiting.clip_id,
                waiting.loaded.track,
                TrackEndReason::Stopped,
            );
            return Ok(());
        }
        let Some(id) = state
            .clips
            .iter()
            .find(|(_, entry)| entry.layer == name && entry.clip_id == clip_id)
            .map(|(&id, _)| id)
        else {
            return Err(ApiError::new(
                ErrorCode::NotFound,
                format!("there is no clip `{clip_id}` on layer `{name}`"),
            ));
        };
        let next = self.is_next(name, id);
        if let Some(layer) = self.mixer().layer_mut(name) {
            layer.deck.end(id, EndReason::Stopped, fade);
        }
        // the deck reports the end of what started; the next clip it forgets silently
        if next {
            let entry = state.clips.remove(&id).expect("found above");
            self.emit_end(name, entry.clip_id, entry.track, TrackEndReason::Stopped);
            self.fill_layer(state, name);
        }
        Ok(())
    }

    /// Ends every clip for a destroy: the deck reports those playing, as cleaned up.
    pub(super) fn end_clips(&self, state: &mut State) {
        let names: Vec<String> = state.layers.iter().map(|l| l.name.clone()).collect();
        for name in names {
            self.drop_unstarted(state, &name, TrackEndReason::Cleanup);
            if let Some(layer) = self.mixer().layer_mut(&name) {
                layer.deck.end_all(EndReason::Stopped, None);
            }
        }
    }

    /// Reports a deck event of the clip `id`; one about a clip already reported ended is late,
    /// and dropped.
    pub(super) fn clip_event(&self, state: &mut State, id: TrackId, event: DeckEvent) {
        let Some(entry) = state.clips.get_mut(&id) else {
            return;
        };
        let (guild_id, layer, clip_id, track) = (
            self.guild_id,
            entry.layer.clone(),
            entry.clip_id.clone(),
            entry.track.clone(),
        );
        match event {
            DeckEvent::Started(_) => {
                entry.started = true;
                self.emit(Event::ClipStart {
                    guild_id,
                    layer: layer.clone(),
                    clip_id,
                    track,
                });
                self.fill_layer(state, &layer);
            }
            DeckEvent::Ended(_, reason) => {
                state.clips.remove(&id);
                let reason = match reason {
                    EndReason::Finished => TrackEndReason::Finished,
                    EndReason::Stopped if state.destroying => TrackEndReason::Cleanup,
                    EndReason::Stopped => TrackEndReason::Stopped,
                    EndReason::Replaced => TrackEndReason::Replaced,
                    EndReason::Failed => TrackEndReason::Failed,
                };
                self.emit_end(&layer, clip_id, track, reason);
                self.fill_layer(state, &layer);
            }
            DeckEvent::Failed(_, error) => self.emit(Event::ClipError {
                guild_id,
                layer,
                clip_id,
                track,
                error: playback_error(&error),
            }),
            DeckEvent::Stuck(_, waiting) => {
                self.emit(Event::ClipStuck {
                    guild_id,
                    layer: layer.clone(),
                    clip_id,
                    track,
                    waiting: Millis::from(waiting),
                });
                // so that the clips after it play; its end follows from the deck
                if let Some(layer) = self.mixer().layer_mut(&layer) {
                    layer.deck.end(id, EndReason::Failed, None);
                }
            }
        }
    }

    /// Every layer but the music's, as clients see it.
    pub(super) fn layer_infos(&self, state: &State) -> Vec<LayerInfo> {
        let mixer = self.mixer();
        state
            .layers
            .iter()
            .map(|layer| {
                let deck = mixer.layer(&layer.name).map(|l| &l.deck);
                let playing = state
                    .clips
                    .iter()
                    .filter(|(_, entry)| entry.layer == layer.name)
                    .map(|(&id, entry)| ClipInfo {
                        clip_id: entry.clip_id.clone(),
                        layer: layer.name.clone(),
                        track: entry.track.clone(),
                        status: if entry.started {
                            ClipStatus::Playing
                        } else {
                            ClipStatus::Waiting
                        },
                        position: Millis::from_ticks(
                            deck.and_then(|deck| deck.position_of(id)).unwrap_or(0),
                        ),
                    });
                let waiting = layer.waiting.iter().map(|waiting| ClipInfo {
                    clip_id: waiting.clip_id.clone(),
                    layer: layer.name.clone(),
                    track: waiting.loaded.track.clone(),
                    status: ClipStatus::Waiting,
                    position: Millis::from_ticks(waiting.options.position.unwrap_or(0)),
                });
                LayerInfo {
                    name: layer.name.clone(),
                    mode: layer.mode,
                    max_clips: layer.max_clips,
                    volume: layer.volume,
                    clipping: layer.clipping,
                    paused: layer.paused,
                    graph: layer.graph.clone(),
                    duck: layer.duck.clone(),
                    clips: playing.chain(waiting).collect(),
                }
            })
            .collect()
    }

    fn layer_info(&self, name: &str) -> LayerInfo {
        let state = self.state();
        self.layer_infos(&state)
            .into_iter()
            .find(|layer| layer.name == name)
            .expect("the layer was just changed")
    }

    /// Opens a clip and puts it on its layer's deck.
    fn place(
        &self,
        node: &Node,
        state: &mut State,
        name: &str,
        clip_id: String,
        loaded: Loaded,
        options: ClipOptions,
    ) -> Placed {
        let mode = layer(state, name).deck_mode();
        let track = loaded.track.clone();
        let clip = self.clip(node, loaded, options);
        let id = clip.track.id;
        state.clips.insert(
            id,
            ClipEntry {
                clip_id,
                layer: name.to_owned(),
                track,
                started: false,
            },
        );
        let mut mixer = self.mixer();
        let deck = &mut mixer.layer_or_add(name).deck;
        deck.place(clip, mode)
            .expect("callers check that the deck has room")
    }

    /// Moves waiting clips onto their layer's deck while it has room.
    fn fill_layer(&self, state: &mut State, name: &str) {
        let Some(node) = self.node.upgrade() else {
            return;
        };
        loop {
            let Some(layer) = state.layers.iter().find(|layer| layer.name == name) else {
                return;
            };
            let mode = layer.deck_mode();
            let has_room = self
                .mixer()
                .layer(name)
                .is_some_and(|layer| layer.deck.has_room(mode));
            if !has_room {
                return;
            }
            let Some(waiting) = layer_mut(state, name).waiting.pop_front() else {
                return;
            };
            self.place(
                &node,
                state,
                name,
                waiting.clip_id,
                waiting.loaded,
                waiting.options,
            );
        }
    }

    /// Ends the clips of a layer that never started: waiting ones, and the next one on its
    /// deck, which the deck forgets without a word.
    fn drop_unstarted(&self, state: &mut State, name: &str, reason: TrackEndReason) {
        let next = self
            .mixer()
            .layer(name)
            .and_then(|layer| layer.deck.next().map(|track| track.id));
        if let Some(id) = next
            && let Some(entry) = state.clips.remove(&id)
        {
            if let Some(layer) = self.mixer().layer_mut(name) {
                layer.deck.end(id, EndReason::Stopped, None);
            }
            self.emit_end(name, entry.clip_id, entry.track, reason);
        }
        let waiting = std::mem::take(&mut layer_mut(state, name).waiting);
        for waiting in waiting {
            self.emit_end(name, waiting.clip_id, waiting.loaded.track, reason);
        }
    }

    /// Whether `id` waits in its layer's deck for the clip playing there.
    fn is_next(&self, name: &str, id: TrackId) -> bool {
        self.mixer()
            .layer(name)
            .and_then(|layer| layer.deck.next())
            .is_some_and(|track| track.id == id)
    }

    fn clip(&self, node: &Node, loaded: Loaded, options: ClipOptions) -> Clip {
        // a sound that cannot be had now is not worth waiting for
        let config = PlayerConfig {
            max_producer_retries: 1,
            ..node.ctx.player_config()
        };
        Clip {
            track: DeckTrack {
                id: self.next_track.fetch_add(1, Ordering::Relaxed) | CLIP_ID,
                length: loaded.track.info.length.map(Millis::to_ticks),
                player: Player::new(Handle::current(), loaded.producer, config, options.position),
                start: options.position.unwrap_or(0),
                end_at: options.end_at,
            },
            volume: options.volume,
            fade_in: options.fade_in,
            fade_out: options.fade_out,
            looping: options.looping,
        }
    }

    /// Plays an already loaded clip, as `clip.play` does once it loaded; for tests to play what
    /// no source loads, such as a stalled producer.
    #[cfg(test)]
    pub(super) async fn play_loaded(
        &self,
        node: &Node,
        layer: &str,
        clip_id: &str,
        loaded: Loaded,
    ) -> Result<ClipInfo, ApiError> {
        let options = ClipOptions {
            volume: 1.0,
            ..ClipOptions::default()
        };
        self.add_clip(node, layer, Some(clip_id.to_owned()), loaded, options)
            .await
    }

    fn emit_end(&self, layer: &str, clip_id: String, track: Track, reason: TrackEndReason) {
        self.emit(Event::ClipEnd {
            guild_id: self.guild_id,
            layer: layer.to_owned(),
            clip_id,
            track,
            reason,
        });
    }
}

fn check_layer_name(name: &str) -> Result<(), ApiError> {
    if name == MAIN {
        return Err(bad_request(
            "`main` is the music's layer: change it with player.update; clips play on layers of \
             their own",
        ));
    }
    let valid = (1..=MAX_LAYER_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(bad_request(format!(
            "a layer's name is 1 to {MAX_LAYER_NAME} of `a-z`, `0-9`, `_` and `-`"
        )))
    }
}

fn check_room_for_layer(state: &State) -> Result<(), ApiError> {
    if state.layers.len() >= MAX_LAYERS {
        return Err(bad_request(format!(
            "a player has at most {MAX_LAYERS} layers besides the music"
        )));
    }
    Ok(())
}

fn check_duck(state: &State, name: &str, duck: &Duck) -> Result<(), ApiError> {
    if duck.volume > 100 {
        return Err(bad_request("duck.volume must be 0 to 100"));
    }
    if [duck.attack, duck.release, duck.hold]
        .into_iter()
        .flatten()
        .any(|time| time > MAX_FADE)
    {
        return Err(bad_request(format!(
            "duck.attack, release and hold take at most {} ms",
            MAX_FADE.0
        )));
    }
    if duck.layers.is_empty() {
        return Err(bad_request("duck.layers names no layer to lower"));
    }
    for target in &duck.layers {
        if target == name {
            return Err(bad_request("a layer cannot duck itself"));
        }
        if target != MAIN && !state.layers.iter().any(|layer| &layer.name == target) {
            return Err(no_layer(target));
        }
    }
    Ok(())
}

fn has_clips(state: &State, name: &str) -> bool {
    state.clips.values().any(|entry| entry.layer == name) || !layer(state, name).waiting.is_empty()
}

fn clip_in_use(state: &State, clip_id: &str) -> bool {
    state.clips.values().any(|entry| entry.clip_id == clip_id)
        || state
            .layers
            .iter()
            .any(|layer| layer.waiting.iter().any(|w| w.clip_id == clip_id))
}

fn layer<'a>(state: &'a State, name: &str) -> &'a LayerState {
    state
        .layers
        .iter()
        .find(|layer| layer.name == name)
        .expect("checked to exist")
}

fn layer_mut<'a>(state: &'a mut State, name: &str) -> &'a mut LayerState {
    state
        .layers
        .iter_mut()
        .find(|layer| layer.name == name)
        .expect("checked to exist")
}

fn deck_duck(duck: &Duck) -> mixer::Duck {
    mixer::Duck {
        layers: duck.layers.clone(),
        gain: f32::from(duck.volume) / 100.0,
        attack: frames(duck.attack.unwrap_or(DEFAULT_ATTACK)),
        release: frames(duck.release.unwrap_or(DEFAULT_RELEASE)),
        hold: frames(duck.hold.unwrap_or(DEFAULT_HOLD)),
    }
}

/// A length of time in frames of the output, which are the engine's ticks.
fn frames(time: Millis) -> usize {
    usize::try_from(time.to_ticks()).unwrap_or(0)
}

fn no_layer(name: &str) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("there is no layer `{name}`"))
}
