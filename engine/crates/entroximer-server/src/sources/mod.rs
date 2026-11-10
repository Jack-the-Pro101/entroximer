//! Turning user input (links, searches, local paths) into tracks with consolidated metadata,
//! and tracks into playable media. See [`provider`] for how input is routed.
//!
//! Loading is lazy: searches and playlists come back with metadata only, and a track is
//! resolved into something playable when it is played or preloaded. Loading for a queue is lazier
//! still (see [`Listing`]): a track a provider can describe without resolving it (a Spotify link)
//! is only described.

pub mod artwork;
pub mod background;
mod direct;
pub mod egress;
pub mod local;
pub mod metadata;
pub(crate) mod net;
pub mod provider;
pub mod spotify;
pub mod upload;
pub mod ytdlp;

use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use entroximer_core::producer::Producer;
use serde::{Deserialize, Serialize};

pub use metadata::{PlaylistInfo, TrackInfo};
pub use provider::{
    DEFAULT_SEARCH_LIMIT, MAX_SEARCH_RESULTS, ProviderDescriptor, SearchOutcome, Sources,
};

use crate::managed_ytdlp::InstallError;
use crate::youtube::{SessionError, SignInUnavailable};
use spotify::SpotifyError;
use ytdlp::{Failure, Restriction};

/// What is needed to load a track again: its provider and its URI there.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackRef {
    /// The provider's name.
    pub source: String,
    pub uri: String,
}

/// The source of a queue's stand-ins for queries it has not loaded yet (see `node::queue`).
/// Resolving one loads its query and plays what that finds.
pub const QUERY_SOURCE: &str = "query";

/// How much of a playlist to list, and whether single tracks are resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Listing {
    /// As `tracks.load` lists: a link to one track resolved, a playlist's every track (up to the
    /// engine's limit).
    #[default]
    Whole,
    /// For a queue: a playlist's tracks from `start_index` (else from the one its link points
    /// at, else from its first), at most `limit` of them (else the engine's limit); tracks
    /// before the start are left out. A provider that can describe a single track without
    /// resolving it does so.
    Queue {
        start_index: Option<usize>,
        limit: Option<usize>,
    },
}

/// A track known by its metadata, e.g. a search result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundTrack {
    pub reference: TrackRef,
    pub info: TrackInfo,
}

/// A track ready to play.
pub struct ResolvedTrack {
    pub track: FoundTrack,
    pub producer: Arc<dyn Producer>,
    /// When the media URLs it plays from stop working, if the source says so (YouTube's work
    /// for six hours, SoundCloud's for about two).
    pub valid_until: Option<SystemTime>,
}

impl std::fmt::Debug for ResolvedTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedTrack")
            .field("track", &self.track)
            .field("caps", &self.producer.caps())
            .finish_non_exhaustive()
    }
}

/// What loading a link or a search produced.
#[derive(Debug)]
pub enum LoadOutcome {
    /// A link to one track, already resolved.
    Track(ResolvedTrack),
    /// A link to one track, described but not resolved; only for [`Listing::Queue`].
    Described(FoundTrack),
    Playlist {
        playlist: PlaylistInfo,
        tracks: Vec<FoundTrack>,
    },
    Search(Vec<FoundTrack>),
    Empty,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("yt-dlp is unavailable")]
    YtDlpUnavailable(#[source] InstallError),
    #[error("failed to run {0}")]
    Spawn(&'static str, #[source] io::Error),
    #[error("yt-dlp could not load `{query}`: {message}")]
    Extraction {
        query: String,
        message: String,
        failure: Failure,
    },
    #[error("this video {}; {}", restriction.describe(), reason.advice())]
    SignInRequired {
        query: String,
        restriction: Restriction,
        reason: SignInUnavailable,
    },
    #[error("cannot use the YouTube session")]
    Session(#[source] SessionError),
    #[error("{0} returned unexpected JSON")]
    Json(&'static str, #[source] serde_json::Error),
    #[error("no results for `{0}`")]
    NoResults(String),
    #[error("the playlist has only {total} tracks")]
    PastEnd { total: u32 },
    #[error("`{0}` has no playable audio")]
    NoAudio(String),
    #[error("no source can play `{0}`")]
    Unsupported(String),
    #[error("`{0}` is not a known search; see the providers in `node.info`")]
    UnknownSearch(String),
    #[error("tracks from `{0}` cannot be loaded by this engine")]
    UnknownSource(String),
    #[error("`{0}` is on this machine or a private network, which the engine does not fetch")]
    PrivateAddress(String),
    #[error("cannot look up `{host}`")]
    Lookup {
        host: String,
        #[source]
        source: io::Error,
    },
    #[error("cannot fetch `{url}`")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("`{url}` answered HTTP {status}")]
    HttpStatus { url: String, status: u16 },
    #[error("ffprobe cannot read `{target}`: {message}")]
    Probe { target: String, message: String },
    #[error("{program} took longer than {} s", after.as_secs())]
    Timeout {
        program: &'static str,
        after: Duration,
    },
    #[error(transparent)]
    Spotify(#[from] SpotifyError),
    #[error("found no recording of `{0}` on YouTube")]
    NoMirror(String),
    #[error("the upload `{0}` was deleted or has expired")]
    UploadGone(String),
    #[error("`{0}` is not a file the engine may play")]
    NoLocalFile(String),
}
