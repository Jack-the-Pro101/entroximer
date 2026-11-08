//! The client protocol: JSON messages over one WebSocket at `/v1/ws`.
//!
//! Inspired by Lavalink v4 but not compatible with it. One ordered stream carries the client's
//! requests, their responses and the engine's events, so a response is never overtaken by an
//! event it caused (events a request causes are sent before its response).
//!
//! - Client to engine: `{"id": 1, "op": "player.update", "params": {...}}`, see [`Call`].
//! - Engine to client: [`ServerMessage`], i.e. `ready`, `response` and `event` messages.
//!
//! These types are exported to TypeScript for the client (`cargo test` writes them to
//! `client/src/generated`).

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use entroximer_core::dsp::{GraphSpec, NodeDescriptor, PresetDescriptor, PresetSelection};
use entroximer_core::timeline::{TICKS_PER_SECOND, Ticks};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use ts_rs::TS;

use crate::node::queue::{QueueChange, QueueItem, Repeat};
use crate::sources::{PlaylistInfo, ProviderDescriptor, TrackInfo};
use crate::youtube::SessionState;

/// Version of this protocol, reported in `node.info`.
pub const PROTOCOL_VERSION: u32 = 1;

/// A Discord ID. Sent as a string, since JavaScript numbers cannot hold every ID exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, TS)]
#[ts(type = "string")]
pub struct Snowflake(pub u64);

impl Serialize for Snowflake {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Snowflake {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map(Self)
            .map_err(|_| serde::de::Error::custom(format!("`{text}` is not a Discord ID")))
    }
}

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Milliseconds.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS,
)]
#[ts(type = "number")]
pub struct Millis(pub u64);

impl Millis {
    const TICKS_PER_MILLI: Ticks = TICKS_PER_SECOND / 1000;

    /// A media position in the engine's 48 kHz ticks; negative ones are zero.
    pub fn from_ticks(ticks: Ticks) -> Self {
        Self(u64::try_from(ticks / Self::TICKS_PER_MILLI).unwrap_or(0))
    }

    pub fn to_ticks(self) -> Ticks {
        i64::try_from(self.0)
            .unwrap_or(i64::MAX)
            .saturating_mul(Self::TICKS_PER_MILLI)
    }

    /// A length in seconds as sources report it; `None` unless finite and positive.
    pub fn from_secs_f64(seconds: f64) -> Option<Self> {
        (seconds.is_finite() && seconds > 0.0).then_some(Self((seconds * 1000.0) as u64))
    }

    /// As people read times in a track: `m:ss`, or `h:mm:ss` from an hour, plus milliseconds
    /// if there are any (`1:05.250`).
    pub fn clock(self) -> String {
        let seconds = self.0 / 1000;
        let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
        let whole = if hours > 0 {
            format!("{hours}:{minutes:02}:{seconds:02}")
        } else {
            format!("{minutes}:{seconds:02}")
        };
        match self.0 % 1000 {
            0 => whole,
            millis => format!("{whole}.{millis:03}"),
        }
    }

    /// The current time as Unix milliseconds.
    pub fn now() -> Self {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(Self(0), Self::from)
    }
}

impl From<Duration> for Millis {
    fn from(duration: Duration) -> Self {
        Self(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A request from the client. `id` is echoed in the response.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Value,
}

/// Everything a client can ask for, by `op`.
#[derive(Debug, Deserialize, TS)]
#[serde(tag = "op", content = "params")]
#[ts(export)]
pub enum Call {
    /// Loads a link, a search (`ytsearch:terms`, or plain text with the engine's default
    /// search) or, if the engine allows it, a local path. Result: [`LoadResult`].
    #[serde(rename = "tracks.load")]
    LoadTrack(LoadTrack),
    /// Searches one source. Result: [`SearchResult`].
    #[serde(rename = "tracks.search")]
    SearchTracks(SearchTracks),
    /// Reads tracks back from their `encoded` form. Result: `Track[]`.
    #[serde(rename = "tracks.decode")]
    DecodeTracks(DecodeTracks),
    /// Builds a preset's effect graph without applying it, e.g. to extend it with more nodes
    /// before setting it as a player's `graph`. Result: [`GraphSpec`].
    #[serde(rename = "presets.build")]
    BuildPreset(PresetSelection),
    /// Result: [`PlayerInfo`].
    #[serde(rename = "player.get")]
    GetPlayer(GuildRef),
    /// The player's signal flow laid out for drawing, one node a row: each layer's source,
    /// effects and volume, joining in the mix. Result: [`PlayerView`].
    #[serde(rename = "player.view")]
    ViewPlayer(ViewPlayer),
    /// Creates the guild's player if needed and changes it. Result: [`PlayerInfo`].
    #[serde(rename = "player.update")]
    UpdatePlayer(Box<PlayerUpdate>),
    /// Stops playback and leaves the voice channel. Result: `null`.
    #[serde(rename = "player.destroy")]
    DestroyPlayer(GuildRef),
    /// Part of a player's queue. Result: [`QueuePage`].
    #[serde(rename = "queue.get")]
    GetQueue(GetQueue),
    /// Adds tracks to a player's queue, creating the player if needed, and answers at once:
    /// each query is queued as a loading item, which the engine loads in the background in
    /// queue order (the first item ahead of the others) and replaces with what it finds (a
    /// `loaded` change). A query added on its own loads at once instead, as `tracks.load` would
    /// load it. With nothing playing, the first item starts once it is ready, unless
    /// playback was stopped and nothing has played since. A queue holds at most 10 000 items.
    /// Result: [`QueueAdded`].
    #[serde(rename = "queue.add")]
    AddToQueue(AddToQueue),
    /// Removes items; ids not in the queue are ignored. Result: [`QueueState`].
    #[serde(rename = "queue.remove")]
    RemoveFromQueue(RemoveFromQueue),
    /// Result: [`QueueState`].
    #[serde(rename = "queue.move")]
    MoveInQueue(MoveInQueue),
    /// Result: [`QueueState`].
    #[serde(rename = "queue.clear")]
    ClearQueue(GuildRef),
    /// Result: [`QueueState`].
    #[serde(rename = "queue.shuffle")]
    ShuffleQueue(GuildRef),
    /// Ends the current track and plays the first item of the queue, or stops if there is
    /// none. Instant if that item is prepared already; otherwise the current track plays on
    /// until the item is ready. Result: [`PlayerInfo`].
    #[serde(rename = "queue.skip")]
    Skip(Skip),
    /// Result: [`QueueState`].
    #[serde(rename = "queue.configure")]
    ConfigureQueue(ConfigureQueue),
    /// Changes the part of an item's track that plays: of an item in the queue (a loading one
    /// too), reported as an `updated` change, or of the playing one (its `itemId`), shown in the
    /// result's `track`. The playing track plays on from where it is: a new start applies the
    /// next time it plays (repeated), a new end now, and one it has passed ends it at once
    /// (`finished`). Times the track cannot play are refused, as in `queue.add`. Result:
    /// [`PlayerInfo`].
    #[serde(rename = "queue.update")]
    UpdateItem(UpdateItem),
    /// Creates a layer of the player (and the player) if needed, and changes it; a refused
    /// update changes nothing. The music is the player's own layer, which `player.update`
    /// changes. Result: [`LayerInfo`].
    #[serde(rename = "layer.update")]
    UpdateLayer(Box<LayerUpdate>),
    /// Removes a layer, ending its clips (`stopped`). Result: `null`.
    #[serde(rename = "layer.remove")]
    RemoveLayer(LayerRef),
    /// Plays a sound on a layer, outside the queue, creating the layer (and the player) if
    /// needed. Its `clipStart` comes before the response if it starts right away. Clips sent
    /// for the same layer join it in the order they were sent. Result: [`ClipInfo`].
    #[serde(rename = "clip.play")]
    PlayClip(Box<PlayClip>),
    /// Stops a clip of a layer, or all of them, waiting ones too. Result: [`LayerInfo`].
    #[serde(rename = "clip.stop")]
    StopClips(StopClips),
    /// Result: [`SessionSettings`].
    #[serde(rename = "session.configure")]
    ConfigureSession(SessionSettings),
    /// Result: [`NodeInfo`].
    #[serde(rename = "node.info")]
    NodeInfo,
    /// Result: [`Stats`].
    #[serde(rename = "node.stats")]
    Stats,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LoadTrack {
    pub query: String,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SearchTracks {
    pub query: String,
    /// A search prefix from `node.info`'s providers, e.g. `ytmsearch`; the engine's default
    /// search if absent.
    #[serde(default)]
    #[ts(optional)]
    pub prefix: Option<String>,
    /// Up to 50; 10 if absent.
    #[serde(default)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// What `tracks.load` found. Search results and playlist tracks carry metadata only; they are
/// resolved when played, which can fill in missing details.
#[derive(Clone, Debug, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(export)]
pub enum LoadResult {
    Track {
        track: Track,
    },
    Playlist {
        playlist: PlaylistInfo,
        tracks: Vec<Track>,
    },
    Search {
        tracks: Vec<Track>,
    },
    Empty,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchResult {
    /// The search prefix used.
    pub prefix: String,
    /// The provider that searched.
    pub provider: String,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DecodeTracks {
    pub encoded: Vec<String>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct GuildRef {
    pub guild_id: Snowflake,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ViewPlayer {
    pub guild_id: Snowflake,
    /// Layers (`main` for the music) whose effect graphs show as one `graph` row each, instead
    /// of node by node.
    #[serde(default)]
    #[ts(optional)]
    pub collapse: Option<Vec<String>>,
}

/// Changes to a player; absent fields are left alone. Its `paused`, `volume`, `clipping`, `graph`
/// and `preset` are the music's: its other layers, changed with `layer.update`, have their own.
#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlayerUpdate {
    pub guild_id: Snowflake,
    /// Plays this track now, in place of the current one; the queue stays as it is and
    /// continues afterwards. `null` stops playback, and the queue waits until something plays
    /// again. With it, `position` and `endTime` take the place of its `startTime` and `endTime`.
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub track: Option<Option<TrackSelector>>,
    /// With `track`: only start it if nothing is playing.
    #[serde(default)]
    #[ts(optional)]
    pub only_if_idle: Option<bool>,
    /// Seeks the current track.
    #[ts(optional)]
    pub position: Option<Millis>,
    /// Stops the current track at this position; `null` plays it to the end. Its track's
    /// `endTime` changes too, so a repeat ends it there as well.
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub end_time: Option<Option<Millis>>,
    /// Pauses the music; the other layers play on.
    #[ts(optional)]
    pub paused: Option<bool>,
    /// Percent: 100 leaves the audio unchanged, up to 1000.
    #[ts(optional)]
    pub volume: Option<u16>,
    /// Hard clips the audio after the volume, so that a volume above 100 distorts it, for fun,
    /// instead of the limiter keeping it at full scale.
    #[ts(optional)]
    pub clipping: Option<bool>,
    /// The effect graph; `null` removes it.
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub graph: Option<Option<GraphSpec>>,
    /// Replaces the effect graph with a preset's (see `node.info` for the presets). The player's
    /// `graph` then shows the nodes it built, ready to be changed or extended. Not together with
    /// `graph`.
    #[ts(optional)]
    pub preset: Option<PresetSelection>,
    /// The bot's voice connection details, from Discord's voice state and voice server
    /// updates for this guild.
    #[ts(optional)]
    pub voice: Option<VoiceState>,
}

/// A track by its `encoded` form (from `tracks.load` or `tracks.search`), or by a query to load
/// first: a search plays its first result, a playlist link the track it points at (or the one at
/// `startIndex`).
///
/// `startTime` and `endTime` play only part of the track, e.g. from where a link points into it:
/// the track starts at `startTime` and ends (`finished`) at `endTime`, every time it plays, and
/// its events show them. The engine does not read them from links. For a query, they apply to
/// the track it finds, or the first of a playlist's. A track that is a stream, or shorter than
/// `startTime`, is refused.
#[derive(Clone, Debug, Deserialize, TS)]
#[serde(untagged)]
pub enum TrackSelector {
    Encoded {
        encoded: String,
        /// Arbitrary data echoed in this track's events, e.g. who requested it.
        #[serde(default, rename = "userData")]
        #[ts(optional)]
        user_data: Option<Value>,
        /// Where the track starts playing; its beginning if absent. (A track's own `startTime`
        /// is not part of its `encoded` form: pass it again to keep it.)
        #[serde(default, rename = "startTime")]
        #[ts(optional)]
        start_time: Option<Millis>,
        /// Where the track stops; its end if absent.
        #[serde(default, rename = "endTime")]
        #[ts(optional)]
        end_time: Option<Millis>,
    },
    Query {
        query: String,
        /// For a playlist: the index of the track to start from, counted from 0, in place of the
        /// one its link points at. The tracks before it are left out.
        #[serde(default, rename = "startIndex")]
        #[ts(optional)]
        start_index: Option<u32>,
        #[serde(default, rename = "userData")]
        #[ts(optional)]
        user_data: Option<Value>,
        /// Where the track found starts playing.
        #[serde(default, rename = "startTime")]
        #[ts(optional)]
        start_time: Option<Millis>,
        /// Where the track found stops.
        #[serde(default, rename = "endTime")]
        #[ts(optional)]
        end_time: Option<Millis>,
    },
}

impl TrackSelector {
    pub fn user_data(&self) -> Option<&Value> {
        match self {
            Self::Encoded { user_data, .. } | Self::Query { user_data, .. } => user_data.as_ref(),
        }
    }

    /// The part of the track to play, as given.
    pub fn times(&self) -> Times {
        match self {
            Self::Encoded {
                start_time,
                end_time,
                ..
            }
            | Self::Query {
                start_time,
                end_time,
                ..
            } => Times {
                start: *start_time,
                end: *end_time,
            },
        }
    }
}

/// The part of a track that plays: from `start` (its beginning if `None`) to `end` (its end if
/// `None`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Times {
    pub start: Option<Millis>,
    pub end: Option<Millis>,
}

impl Times {
    pub fn of(track: &Track) -> Self {
        Self {
            start: track.start_time,
            end: track.end_time,
        }
    }

    /// Checks the times on their own: the start must come before the end. A start at 0 is the
    /// beginning.
    pub fn checked(self) -> Result<Self, ApiError> {
        let start = self.start.filter(|start| start.0 > 0);
        if let (Some(start), Some(end)) = (start, self.end)
            && start >= end
        {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!(
                    "startTime ({}) must come before endTime ({})",
                    start.clock(),
                    end.clock()
                ),
            ));
        }
        Ok(Self { start, ..self })
    }

    /// Puts the times on `track`, if it can play them: it must not be a stream, and must be
    /// longer than `start` (if its length is known).
    pub fn apply(self, mut track: Track) -> Result<Track, ApiError> {
        let Self { start, end } = self.checked()?;
        let info = &track.info;
        if let Some(start) = start {
            let refusal = if !info.is_seekable {
                Some("it is a stream".to_owned())
            } else {
                info.length
                    .filter(|length| start >= *length)
                    .map(|length| format!("it is only {} long", length.clock()))
            };
            if let Some(refusal) = refusal {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "`{}` cannot start at {}: {refusal}",
                        info.title,
                        start.clock()
                    ),
                ));
            }
        }
        track.start_time = start;
        track.end_time = end;
        Ok(track)
    }

    pub fn start_ticks(self) -> Option<Ticks> {
        self.start.map(Millis::to_ticks)
    }

    pub fn end_ticks(self) -> Option<Ticks> {
        self.end.map(Millis::to_ticks)
    }
}

/// A `startIndex` as an index.
pub fn as_index(start_index: Option<u32>) -> Option<usize> {
    start_index.map(|index| index as usize)
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct GetQueue {
    pub guild_id: Snowflake,
    #[serde(default)]
    #[ts(optional)]
    pub offset: Option<u32>,
    /// Up to 1000; 100 if absent.
    #[serde(default)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AddToQueue {
    pub guild_id: Snowflake,
    /// Added in this order. A query is queued as a loading item, which loads into what it
    /// finds: its track, a playlist's tracks (from `startIndex`), or a search's first result; one
    /// that finds nothing is reported with `trackError` and removed. Encoded tracks are queued as
    /// they are (all or nothing: an invalid one adds none) and resolved when they come up.
    pub tracks: Vec<TrackSelector>,
    /// Where the tracks go; the end if absent or past it.
    #[serde(default)]
    #[ts(optional)]
    pub index: Option<u32>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RemoveFromQueue {
    pub guild_id: Snowflake,
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MoveInQueue {
    pub guild_id: Snowflake,
    pub id: String,
    /// The item's new index, counted without it; the end if past it.
    pub index: u32,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Skip {
    pub guild_id: Snowflake,
    /// Skips to this item, removing the ones before it.
    #[serde(default)]
    #[ts(optional)]
    pub to_item: Option<String>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureQueue {
    pub guild_id: Snowflake,
    #[serde(default)]
    #[ts(optional)]
    pub repeat: Option<Repeat>,
    /// How long the end of each track overlaps the start of the next, up to 12 s; 0 plays them
    /// back to back. At most half of a track fades, and streams never do.
    #[serde(default)]
    #[ts(optional)]
    pub crossfade: Option<Millis>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct UpdateItem {
    pub guild_id: Snowflake,
    /// An item in the queue, or the playing one.
    pub id: String,
    /// Where its track starts playing; `null` from its beginning; absent leaves it.
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub start_time: Option<Option<Millis>>,
    /// Where its track stops; `null` at its end; absent leaves it.
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub end_time: Option<Option<Millis>>,
}

impl UpdateItem {
    /// `times` with the changes asked for.
    pub fn changed(&self, times: Times) -> Times {
        Times {
            start: self.start_time.unwrap_or(times.start),
            end: self.end_time.unwrap_or(times.end),
        }
    }
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LayerRef {
    pub guild_id: Snowflake,
    pub layer: String,
}

/// Changes to a layer; absent fields are left alone.
#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LayerUpdate {
    pub guild_id: Snowflake,
    /// `a-z`, `0-9`, `_` and `-`, up to 32; not `main`, the music's.
    pub layer: String,
    /// Percent: 100 leaves the audio unchanged, up to 1000.
    #[ts(optional)]
    pub volume: Option<u16>,
    /// Hard clips the layer's audio after its volume, as `player.update`'s does the music's.
    #[ts(optional)]
    pub clipping: Option<bool>,
    /// The layer's own effect graph; `null` removes it.
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub graph: Option<Option<GraphSpec>>,
    /// Replaces the effect graph with a preset's. Not together with `graph`.
    #[ts(optional)]
    pub preset: Option<PresetSelection>,
    /// A paused layer holds its clips where they are; the others play on.
    #[ts(optional)]
    pub paused: Option<bool>,
    /// What a clip does to those playing already; only while the layer has none.
    #[ts(optional)]
    pub mode: Option<ClipMode>,
    /// With `mix`: how many clips play at once, 1 to 8.
    #[ts(optional)]
    pub max_clips: Option<u8>,
    /// Lowers other layers while one of this layer's clips is heard; `null` stops it.
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub duck: Option<Option<Duck>>,
}

/// What a new clip does while a layer plays others.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClipMode {
    /// It waits its turn and plays after them, e.g. announcements.
    #[default]
    Queue,
    /// It plays over them, up to `maxClips` at once; beyond that the oldest ends as `replaced`.
    Mix,
    /// It ends them (`replaced`), and the waiting ones.
    Replace,
}

/// How a layer lowers others while one of its clips is heard (playing and not waiting for
/// media), e.g. the music under an announcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Duck {
    /// The layers lowered; `main` is the music.
    pub layers: Vec<String>,
    /// Percent of their volume they keep, 0 to 100.
    pub volume: u16,
    /// How long lowering them takes; 50 ms if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub attack: Option<Millis>,
    /// How long bringing them back takes; 500 ms if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub release: Option<Millis>,
    /// How long they stay lowered after the clips went quiet, so that they do not come back
    /// between two clips; 250 ms if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub hold: Option<Millis>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlayClip {
    pub guild_id: Snowflake,
    pub layer: String,
    pub track: TrackSelector,
    /// The clip's id in its events, up to 64 characters, unique among the player's clips;
    /// one is made up if absent.
    #[serde(default)]
    #[ts(optional)]
    pub clip_id: Option<String>,
    /// Percent of the layer's volume, up to 1000; 100 if absent.
    #[ts(optional)]
    pub volume: Option<u16>,
    /// Where in the track it starts; its `startTime` if absent.
    #[ts(optional)]
    pub position: Option<Millis>,
    /// Where in the track it ends; its `endTime` if absent.
    #[ts(optional)]
    pub end_time: Option<Millis>,
    /// How long it takes to fade in, up to 10 s.
    #[ts(optional)]
    pub fade_in: Option<Millis>,
    /// How long it takes to fade out when stopped or replaced, up to 10 s.
    #[ts(optional)]
    pub fade_out: Option<Millis>,
    /// Plays again from the start whenever it ends, until stopped; not on `queue` layers,
    /// where it would hold up every clip after it.
    #[serde(default, rename = "loop")]
    #[ts(optional, rename = "loop")]
    pub looping: Option<bool>,
}

#[derive(Debug, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StopClips {
    pub guild_id: Snowflake,
    pub layer: String,
    /// The clip to stop; every clip of the layer if absent.
    #[serde(default)]
    #[ts(optional)]
    pub clip_id: Option<String>,
    /// How long they take to fade out, up to 10 s; as each clip says if absent.
    #[ts(optional)]
    pub fade_out: Option<Millis>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct VoiceState {
    pub token: String,
    pub endpoint: String,
    pub session_id: String,
    pub channel_id: Snowflake,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionSettings {
    /// How long players stay paused after the connection drops, waiting for the client to resume
    /// the session; afterwards they are destroyed. 0 destroys them right away; at most a day.
    #[ts(type = "number")]
    pub resume_timeout_secs: u64,
}

/// Messages from the engine.
#[derive(Debug, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(export)]
pub enum ServerMessage {
    /// First message on every connection.
    #[serde(rename_all = "camelCase")]
    Ready {
        session_id: String,
        /// Whether an existing session was resumed. Its players paused when the connection
        /// dropped and continue from the same position now.
        resumed: bool,
        /// Events from while disconnected that could not be kept (the buffer is bounded). The
        /// kept ones follow this message, then a `playerUpdate` for every player.
        #[ts(type = "number")]
        missed_events: u64,
    },
    Response(Response),
    Event(Box<Event>),
}

#[derive(Debug, Serialize, TS)]
pub struct Response {
    // JSON numbers; request ids stay far below 2^53
    #[ts(type = "number")]
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Serialize, TS)]
#[serde(untagged)]
pub enum Outcome {
    Ok { result: Value },
    Err { error: ApiError },
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    /// `itemId` identifies the track's queue item in all its events; tracks played directly
    /// get one too.
    #[serde(rename_all = "camelCase")]
    TrackStart {
        guild_id: Snowflake,
        track: Track,
        item_id: String,
    },
    #[serde(rename_all = "camelCase")]
    TrackEnd {
        guild_id: Snowflake,
        track: Track,
        item_id: String,
        reason: TrackEndReason,
    },
    /// Followed by `trackEnd` with reason `failed` if the track was playing. A queue item that
    /// fails to load, or whose query does (`track` is then its stand-in), is removed from the
    /// queue instead (see `queueChanged`), and the queue goes on with the next one.
    #[serde(rename_all = "camelCase")]
    TrackError {
        guild_id: Snowflake,
        track: Track,
        item_id: String,
        error: ApiError,
    },
    /// The track has been waiting for media longer than the engine's threshold, besides a wait
    /// its source announced (YouTube holds a video's media back during its pre-roll ad);
    /// reported once per stall, with the whole wait. Playback resumes by itself if the media
    /// arrives.
    #[serde(rename_all = "camelCase")]
    TrackStuck {
        guild_id: Snowflake,
        track: Track,
        item_id: String,
        waiting: Millis,
    },
    /// The queue changed. `version` counts changes: if it is not one more than the last one
    /// seen, some were missed and the client should fetch the queue again (`queue.get`).
    #[serde(rename_all = "camelCase")]
    QueueChanged {
        guild_id: Snowflake,
        #[ts(type = "number")]
        version: u64,
        change: QueueChange,
    },
    /// Playback stopped because nothing is left to play.
    #[serde(rename_all = "camelCase")]
    QueueEnd { guild_id: Snowflake },
    /// Sent periodically and after changes.
    #[serde(rename_all = "camelCase")]
    PlayerUpdate {
        guild_id: Snowflake,
        state: PlayerState,
    },
    /// The voice connection closed. `code` is Discord's voice close code, if it sent one.
    #[serde(rename_all = "camelCase")]
    VoiceClosed {
        guild_id: Snowflake,
        #[ts(optional)]
        code: Option<u16>,
        reason: String,
        by_remote: bool,
    },
    /// Sent every minute.
    Stats(Stats),
    /// The YouTube session's state changed, e.g. it expired and needs fresh cookies.
    #[serde(rename_all = "camelCase")]
    YoutubeSession { state: SessionState },
    /// A clip started playing. One that waited its turn and was stopped never starts.
    #[serde(rename_all = "camelCase")]
    ClipStart {
        guild_id: Snowflake,
        layer: String,
        clip_id: String,
        track: Track,
    },
    /// Every clip `clip.play` accepted ends once: `finished`, `stopped`, `replaced`, `failed`
    /// (after `clipError` or `clipStuck`) or `cleanup`.
    #[serde(rename_all = "camelCase")]
    ClipEnd {
        guild_id: Snowflake,
        layer: String,
        clip_id: String,
        track: Track,
        reason: TrackEndReason,
    },
    #[serde(rename_all = "camelCase")]
    ClipError {
        guild_id: Snowflake,
        layer: String,
        clip_id: String,
        track: Track,
        error: ApiError,
    },
    /// The clip waited for media longer than the engine's threshold, besides a wait its source
    /// announced (as for `trackStuck`); it ends as `failed`, so that the clips after it play.
    #[serde(rename_all = "camelCase")]
    ClipStuck {
        guild_id: Snowflake,
        layer: String,
        clip_id: String,
        track: Track,
        waiting: Millis,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TrackEndReason {
    /// Played to the end (or its end time); the next track, if any, has started.
    Finished,
    Stopped,
    /// Another track was started in its place.
    Replaced,
    /// Playback failed; see the preceding `trackError`.
    Failed,
    /// The player was destroyed.
    Cleanup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Track {
    /// Pass this back to play the track. base64url-encoded JSON; stable across engine versions.
    pub encoded: String,
    pub info: TrackInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub user_data: Option<Value>,
    /// Where the track starts playing, as the client asked; absent for its beginning. Its
    /// `info.length` stays the whole track's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub start_time: Option<Millis>,
    /// Where the track stops, as the client asked; absent for its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub end_time: Option<Millis>,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PlayerInfo {
    pub guild_id: Snowflake,
    pub track: Option<Track>,
    /// The current track's queue item.
    pub item_id: Option<String>,
    pub queue: QueueState,
    pub paused: bool,
    pub volume: u16,
    /// Whether the audio is hard clipped after the volume.
    pub clipping: bool,
    /// The effect graph, as set or as a preset built it.
    pub graph: Option<GraphSpec>,
    pub state: PlayerState,
    /// The player's layers besides the music, which the fields above describe.
    pub layers: Vec<LayerInfo>,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LayerInfo {
    pub name: String,
    pub mode: ClipMode,
    pub max_clips: u8,
    pub volume: u16,
    pub clipping: bool,
    pub paused: bool,
    pub graph: Option<GraphSpec>,
    pub duck: Option<Duck>,
    /// The clips playing, then those waiting, in the order they were played.
    pub clips: Vec<ClipInfo>,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ClipInfo {
    pub clip_id: String,
    pub layer: String,
    pub track: Track,
    pub status: ClipStatus,
    /// Position in the clip's track.
    pub position: Millis,
}

/// A player's signal flow as rows to draw from top to bottom, the way `git log --graph` draws
/// history: one node a row, after every node feeding it, with the wires running down a gutter
/// on the left. Each layer starts at its source and ends at its volume, and the layers join in
/// the mix:
///
/// ```text
/// ■      main · Song
/// ●      low · lowpass · cutoff 220
/// ○      volume 80%
/// │ ■    sfx · 2 clips playing
/// │ ○    volume 100%
/// ├─┘
/// □      mix · limiter
/// ```
///
/// Effect graphs that branch fork (`├─┐`) and join (`├─┘`) inside their layer.
#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PlayerView {
    pub guild_id: Snowflake,
    /// The most wires side by side; every gutter is `2 * lanes - 1` characters wide.
    pub lanes: u8,
    pub rows: Vec<ViewRow>,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ViewRow {
    /// The wires, for a monospace font, padded to the same width in every row. Wire `i` runs down
    /// column `2 * i`, as `│`; the columns between carry `─` across. `├` with `┐` (`┬` between)
    /// forks a wire, `├` with `┘` (`┴` between) joins wires, and `┼` is a wire crossing another.
    /// The node sits on its wire as `■` (a source), `●` (an effect), `◊` (a collapsed graph),
    /// `○` (the rate or the volume) or `□` (the mix). Only these characters and spaces appear, so
    /// a client can draw them otherwise.
    pub gutter: String,
    /// `null` on rows where wires only fork or join.
    pub node: Option<ViewNode>,
}

/// One node of a [`PlayerView`]. A layer's `effect` is named by its `layer`, `id` and
/// `graphTag`; the other kinds by their `layer` and `kind`.
#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ViewNode {
    pub kind: ViewNodeKind,
    /// The layer it belongs to, `main` for the music; `null` for the `mix`.
    pub layer: Option<String>,
    /// For an `effect`: its node's id in the layer's graph.
    pub id: Option<String>,
    /// For an `effect` and a `graph`: the tag of the layer's graph, which changes whenever the
    /// graph does. It tells a node picked from a view from one of the same id in a graph that
    /// has replaced it since.
    pub graph_tag: Option<String>,
    /// What it is or plays: a track's title, an effect's node type, `volume 80%`.
    pub label: String,
    /// More about it: an effect's parameters, whether a layer is paused or ducked.
    pub detail: Option<String>,
    /// The rows of the nodes feeding it.
    pub inputs: Vec<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ViewNodeKind {
    /// What a layer plays: the music's current track, or another layer's clips.
    Source,
    /// The playback rate the layer's graph sets.
    Rate,
    /// A node of the layer's effect graph.
    Effect,
    /// The layer's whole effect graph, collapsed into one row.
    Graph,
    /// The layer's volume, and its clipping.
    Volume,
    /// Every layer summed, through the master limiter, into the voice channel.
    Mix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClipStatus {
    Playing,
    /// Waits for those before it on a `queue` layer.
    Waiting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct QueueState {
    /// The number of changes so far, as in `queueChanged`.
    #[ts(type = "number")]
    pub version: u64,
    pub length: u32,
    pub repeat: Repeat,
    pub crossfade: Millis,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct QueuePage {
    pub queue: QueueState,
    pub offset: u32,
    pub items: Vec<QueueItem>,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct QueueAdded {
    /// Where the items went.
    pub index: u32,
    pub items: Vec<QueueItem>,
    pub queue: QueueState,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    /// When this state was taken, in Unix milliseconds.
    pub time: Millis,
    /// Position in the current track.
    pub position: Millis,
    /// Whether the engine is connected to the voice channel.
    pub connected: bool,
    /// Whether the current track is waiting for media (loading, or seeking ahead of it).
    pub buffering: bool,
    /// Downloaded parts of the current track, as `[start, end]` pairs.
    pub buffered: Vec<[Millis; 2]>,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Stats {
    pub players: u32,
    pub playing_players: u32,
    pub uptime: Millis,
    /// Resident memory of the engine process, in bytes.
    #[ts(type = "number")]
    pub memory_bytes: u64,
    /// Cached audio held in RAM and on disk by all players, in bytes.
    #[ts(type = "number")]
    pub cache_memory_bytes: u64,
    #[ts(type = "number")]
    pub cache_disk_bytes: u64,
    /// The most cached audio that all players may hold in RAM, in bytes.
    #[ts(type = "number")]
    pub cache_memory_limit_bytes: u64,
    /// The most that all players may hold on disk, in bytes; 0 if the engine keeps cached audio
    /// in RAM only.
    #[ts(type = "number")]
    pub cache_disk_limit_bytes: u64,
    /// Tracks of repeated queues kept on disk for their next round, in bytes.
    #[ts(type = "number")]
    pub loop_cache_bytes: u64,
    /// The most that tracks kept for repeated queues may take on disk, in bytes; 0 keeps none.
    #[ts(type = "number")]
    pub loop_cache_limit_bytes: u64,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct NodeInfo {
    pub version: String,
    pub protocol: u32,
    /// Version of the yt-dlp the engine runs, if it can tell.
    pub ytdlp: Option<String>,
    pub youtube_session: SessionState,
    /// Where tracks can come from, with their search prefixes.
    pub providers: Vec<ProviderDescriptor>,
    /// Effect node types usable in graphs.
    pub nodes: Vec<NodeDescriptor>,
    /// Ready-made effect graphs, with the graph each builds at its default parameters.
    pub presets: Vec<PresetDescriptor>,
    /// What `POST /v1/uploads` takes; `null` if uploads are off.
    pub uploads: Option<UploadsInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UploadsInfo {
    /// The largest file, in bytes.
    #[ts(type = "number")]
    pub max_bytes: u64,
    /// How long an upload is kept after it was uploaded or last used.
    pub keep: Millis,
}

/// Result of `POST /v1/uploads`.
#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Upload {
    /// For `DELETE /v1/uploads/{id}`.
    pub id: String,
    /// Plays like any other track; its `uri` is `upload:<id>`, which also loads it.
    pub track: Track,
    #[ts(type = "number")]
    pub bytes: u64,
    /// When it is deleted, as Unix milliseconds, unless it is used before: each use keeps it for
    /// `keep` (see `UploadsInfo`) longer.
    pub expires_at: Millis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

/// Stable, machine-readable error kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The `Authorization` header is missing or wrong.
    Unauthorized,
    BadRequest,
    NotFound,
    /// The query matched nothing.
    NoResults,
    /// The media exists but has no playable audio.
    NoAudio,
    /// Removed, private to others, geo-blocked or never existed.
    Unavailable,
    AgeRestricted,
    MembersOnly,
    Private,
    BotCheck,
    SignInRequired,
    RateLimited,
    /// Loading failed for another reason (see the message).
    LoadFailed,
    /// Playback failed after the track loaded.
    PlaybackFailed,
    /// An upload is larger than the engine takes.
    TooLarge,
    /// The engine has no room for an upload until others expire or are deleted.
    StorageFull,
    Internal,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Distinguishes an absent field (`None`) from an explicit `null` (`Some(None)`).
fn present<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn calls_parse_by_op() {
        let call: Call = serde_json::from_value(json!({
            "op": "player.update",
            "params": { "guildId": "123", "track": null, "volume": 50 }
        }))
        .unwrap();
        let Call::UpdatePlayer(update) = call else {
            panic!("wrong call");
        };
        assert_eq!(update.guild_id, Snowflake(123));
        assert!(matches!(update.track, Some(None)), "explicit null stops");
        assert!(update.position.is_none(), "absent is left alone");

        let call: Call = serde_json::from_value(json!({
            "op": "queue.add",
            "params": { "guildId": "123", "tracks": [{ "query": "ytsearch:x", "userData": { "by": "me" } }, { "encoded": "abc" }, { "query": "https://example.com/list", "startIndex": 4 }] }
        }))
        .unwrap();
        let Call::AddToQueue(add) = call else {
            panic!("wrong call");
        };
        assert!(
            matches!(&add.tracks[0], TrackSelector::Query { query, .. } if query == "ytsearch:x")
        );
        assert_eq!(add.tracks[0].user_data(), Some(&json!({ "by": "me" })));
        assert!(matches!(&add.tracks[1], TrackSelector::Encoded { .. }));
        assert!(matches!(
            &add.tracks[2],
            TrackSelector::Query {
                start_index: Some(4),
                ..
            }
        ));
        assert_eq!(add.index, None);

        let call: Call = serde_json::from_value(
            json!({ "op": "queue.configure", "params": { "guildId": "1", "repeat": "queue" } }),
        )
        .unwrap();
        assert!(matches!(
            call,
            Call::ConfigureQueue(ConfigureQueue {
                repeat: Some(Repeat::Queue),
                ..
            })
        ));

        let call: Call = serde_json::from_value(json!({
            "op": "queue.update",
            "params": { "guildId": "1", "id": "a", "startTime": 1000, "endTime": null }
        }))
        .unwrap();
        let Call::UpdateItem(update) = call else {
            panic!("wrong call");
        };
        let times = Times {
            start: Some(Millis(5)),
            end: Some(Millis(9000)),
        };
        assert_eq!(
            update.changed(times),
            Times {
                start: Some(Millis(1000)),
                end: None
            },
            "null clears, a time replaces"
        );
        let update: UpdateItem =
            serde_json::from_value(json!({ "guildId": "1", "id": "a" })).unwrap();
        assert_eq!(update.changed(times), times, "absent leaves them");

        let call: Call = serde_json::from_value(json!({
            "op": "player.update",
            "params": { "guildId": "1", "preset": { "name": "bass_boost", "params": { "gain_db": 9 } } }
        }))
        .unwrap();
        let Call::UpdatePlayer(update) = call else {
            panic!("wrong call");
        };
        let preset = update.preset.unwrap();
        assert_eq!(preset.name, "bass_boost");
        assert_eq!(preset.params.unwrap()["gain_db"], 9);
        assert!(update.graph.is_none());

        let call: Call = serde_json::from_value(json!({ "op": "node.stats" })).unwrap();
        assert!(matches!(call, Call::Stats));
        assert!(serde_json::from_value::<Call>(json!({ "op": "nope" })).is_err());
        assert!(serde_json::from_value::<Snowflake>(json!("12a")).is_err());
    }

    #[test]
    fn selectors_take_the_part_of_the_track_to_play() {
        let selector: TrackSelector = serde_json::from_value(
            json!({ "query": "https://example.com/a", "startTime": 90_000, "endTime": 120_000 }),
        )
        .unwrap();
        assert_eq!(
            selector.times(),
            Times {
                start: Some(Millis(90_000)),
                end: Some(Millis(120_000))
            }
        );
        let selector: TrackSelector =
            serde_json::from_value(json!({ "encoded": "abc", "endTime": 5000 })).unwrap();
        assert_eq!(selector.times().end, Some(Millis(5000)));

        let backwards = Times {
            start: Some(Millis(5000)),
            end: Some(Millis(5000)),
        };
        assert!(backwards.checked().is_err());
        let from_zero = Times {
            start: Some(Millis(0)),
            end: Some(Millis(10)),
        };
        assert_eq!(
            from_zero.checked().unwrap().start,
            None,
            "0 is the beginning"
        );
    }

    #[test]
    fn times_read_as_clock_times() {
        assert_eq!(Millis(0).clock(), "0:00");
        assert_eq!(Millis(65_250).clock(), "1:05.250");
        assert_eq!(Millis(3_723_000).clock(), "1:02:03");
    }

    #[test]
    fn millis_convert_to_and_from_ticks() {
        assert_eq!(Millis(1500).to_ticks(), 72_000);
        assert_eq!(Millis::from_ticks(72_047), Millis(1500));
        assert_eq!(Millis::from_ticks(-5), Millis(0));
        assert_eq!(Millis::from_secs_f64(2.5), Some(Millis(2500)));
        assert_eq!(Millis::from_secs_f64(f64::NAN), None);
        assert_eq!(Millis::from(Duration::from_secs(3)), Millis(3000));
    }

    #[test]
    fn messages_serialize_flat() {
        let msg = ServerMessage::Response(Response {
            id: 7,
            outcome: Outcome::Err {
                error: ApiError::new(ErrorCode::AgeRestricted, "nope"),
            },
        });
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({ "type": "response", "id": 7, "error": { "code": "age_restricted", "message": "nope" } })
        );
        let ready = ServerMessage::Ready {
            session_id: "s".into(),
            resumed: false,
            missed_events: 0,
        };
        assert_eq!(
            serde_json::to_value(&ready).unwrap(),
            json!({ "type": "ready", "sessionId": "s", "resumed": false, "missedEvents": 0 })
        );
        let event = ServerMessage::Event(Box::new(Event::YoutubeSession {
            state: SessionState::Expired,
        }));
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({ "type": "event", "event": "youtubeSession", "state": "expired" })
        );
    }
}
