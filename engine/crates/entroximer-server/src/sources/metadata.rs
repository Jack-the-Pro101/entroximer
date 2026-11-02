//! One metadata model for every source. Providers fill it from whatever their site reports;
//! clients see the same fields whether a track came from YouTube, SoundCloud or a file.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::api::protocol::Millis;

/// What clients are told about a track. Values a source does not provide are `null`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TrackInfo {
    /// The source's own ID for the track (a video ID, a track number, or the URI).
    pub identifier: String,
    pub title: String,
    /// `null` while unknown, e.g. for YouTube Music search results until the track is resolved.
    pub author: Option<String>,
    /// The author's page.
    pub author_url: Option<String>,
    pub album: Option<String>,
    /// `null` when unknown, and for streams.
    pub length: Option<Millis>,
    pub is_stream: bool,
    /// Every track but a live stream can seek.
    pub is_seekable: bool,
    /// The track's page, or the file's URL or path.
    pub uri: String,
    /// A picture of the track: a web URL, or, for the cover embedded in a media file, a path on
    /// the engine (`/v1/artwork/...`) that needs the engine's `Authorization`. From the source,
    /// else the playlist's or the web page's picture, else the embedded cover.
    pub artwork_url: Option<String>,
    /// The International Standard Recording Code, where the source knows it.
    pub isrc: Option<String>,
    /// Where the track comes from, e.g. `youtube`, `soundcloud`, `http`.
    pub source_name: String,
}

/// What clients are told about a playlist; its tracks come alongside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PlaylistInfo {
    pub name: String,
    pub author: Option<String>,
    pub uri: Option<String>,
    pub artwork_url: Option<String>,
    /// The track the link pointed at, as an index into the tracks, e.g. from YouTube's `v=`.
    pub selected_track: Option<u32>,
    /// How many tracks the playlist has; more than were loaded if it exceeded the engine's limit.
    pub total_tracks: Option<u32>,
}
