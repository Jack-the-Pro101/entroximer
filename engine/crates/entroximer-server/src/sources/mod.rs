//! Turning user input (links, searches, local paths) into tracks with consolidated metadata,
//! and tracks into playable media. See [`provider`] for how input is routed.
//!
//! Loading is lazy: searches and playlists come back with metadata only, and a track is
//! resolved into something playable when it is played or preloaded.

pub mod artwork;
mod direct;
pub mod local;
pub mod metadata;
mod net;
pub mod provider;
pub mod spotify;
pub mod upload;
pub mod ytdlp;

use std::io;
use std::sync::Arc;
use std::time::Duration;

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
