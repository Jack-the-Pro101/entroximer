//! Tracks as clients see them: loading and searching, the `encoded` form, and short-lived caches
//! so that playing a track right after loading it, or repeating a search (e.g. while a user
//! types into an autocomplete field), does not ask the source again.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use entroximer_core::error::display_chain;
use entroximer_core::producer::Producer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::protocol::{ApiError, ErrorCode, LoadResult, SearchResult, Track};
use crate::config::MediaContext;
use crate::expiring::Expiring;
use crate::sources::spotify::SpotifyError;
use crate::sources::upload;
use crate::sources::ytdlp::{Failure, Restriction};
use crate::sources::{
    FoundTrack, LoadOutcome, QUERY_SOURCE, ResolveError, ResolvedTrack, TrackInfo, TrackRef,
};

/// Version of the `encoded` form.
const ENCODING_VERSION: u8 = 2;
/// Results a search returns unless asked otherwise.
const DEFAULT_SEARCH_RESULTS: u32 = 10;
/// How long resolved tracks are reused at most, which saves extracting them again (YouTube's
/// most costly requests) when they play again, e.g. in a repeated queue. An entry is a few KiB (up
/// to about 100 KiB for a video that only streams over SABR).
const KEEP_RESOLVED_FOR: Duration = Duration::from_secs(4 * 60 * 60);
/// How long a resolved track whose media URLs do not say when they stop working is reused.
const KEEP_UNDATED_FOR: Duration = Duration::from_secs(60 * 60);
/// Reuse ends this long before a track's media URLs stop working, so that one played just before
/// still downloads, and seeks, while they work.
const URL_MARGIN: Duration = Duration::from_secs(30 * 60);
/// How long search results are reused, e.g. while a user types into an autocomplete field.
const KEEP_SEARCHES_FOR: Duration = Duration::from_secs(10 * 60);
const CACHE_CAPACITY: usize = 256;

/// The `encoded` form: where to load the track again, plus its info for decoding.
#[derive(Serialize, Deserialize)]
struct Encoded {
    v: u8,
    #[serde(flatten)]
    reference: TrackRef,
    info: TrackInfo,
}

pub fn encode(track: &FoundTrack) -> String {
    let encoded = Encoded {
        v: ENCODING_VERSION,
        reference: track.reference.clone(),
        info: track.info.clone(),
    };
    let json = serde_json::to_vec(&encoded).expect("track info serializes");
    URL_SAFE_NO_PAD.encode(json)
}

pub fn decode(encoded: &str) -> Result<FoundTrack, ApiError> {
    let invalid = |why: &str| {
        ApiError::new(
            ErrorCode::BadRequest,
            format!("invalid encoded track: {why}"),
        )
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .map_err(|_| invalid("not base64url"))?;
    let decoded: Encoded = serde_json::from_slice(&bytes).map_err(|_| invalid("not a track"))?;
    if decoded.v != ENCODING_VERSION {
        return Err(invalid("from an incompatible engine version"));
    }
    Ok(FoundTrack {
        reference: decoded.reference,
        info: decoded.info,
    })
}

/// A track as clients see it, from its `encoded` form.
pub fn decode_track(encoded: &str) -> Result<Track, ApiError> {
    Ok(Track {
        encoded: encoded.to_owned(),
        info: decode(encoded)?.info,
        user_data: None,
    })
}

/// A track as clients see it.
pub fn to_track(found: FoundTrack) -> Track {
    Track {
        encoded: encode(&found),
        info: found.info,
        user_data: None,
    }
}

/// A queue's stand-in for a query it has not loaded yet, shown as the query. Played, or queued
/// again, it loads the query.
pub fn stand_in(query: &str, user_data: Option<&Value>) -> Track {
    let found = FoundTrack {
        reference: TrackRef {
            source: QUERY_SOURCE.to_owned(),
            uri: query.to_owned(),
        },
        info: TrackInfo {
            identifier: query.to_owned(),
            title: query.to_owned(),
            author: None,
            author_url: None,
            album: None,
            length: None,
            is_stream: false,
            is_seekable: true,
            uri: query.to_owned(),
            artwork_url: None,
            isrc: None,
            source_name: QUERY_SOURCE.to_owned(),
        },
    };
    Track {
        user_data: user_data.cloned(),
        ..to_track(found)
    }
}

/// A track ready to play.
#[derive(Clone)]
pub struct Loaded {
    pub track: Track,
    pub producer: Arc<dyn Producer>,
    /// When its media URLs stop working, if known.
    pub valid_until: Option<SystemTime>,
}

impl From<ResolvedTrack> for Loaded {
    fn from(resolved: ResolvedTrack) -> Self {
        Self {
            track: to_track(resolved.track),
            producer: resolved.producer,
            valid_until: resolved.valid_until,
        }
    }
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("track", &self.track.info.title)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct TrackCache {
    /// Resolved tracks by `encoded` form.
    resolved: Expiring<String, Loaded>,
    /// Search results by search prefix, lowercased terms and limit.
    searches: Expiring<(String, String, u32), SearchResult>,
}

impl Default for TrackCache {
    fn default() -> Self {
        Self {
            resolved: Expiring::new(KEEP_RESOLVED_FOR, CACHE_CAPACITY),
            searches: Expiring::new(KEEP_SEARCHES_FOR, CACHE_CAPACITY),
        }
    }
}

impl TrackCache {
    /// Loads a link, a path or a search. A single track is resolved right away and kept, so
    /// playing it next costs nothing.
    pub async fn load(&self, ctx: &MediaContext, query: &str) -> Result<LoadResult, ApiError> {
        let outcome = ctx
            .sources
            .load(query, ctx)
            .await
            .map_err(|e| load_error(&e))?;
        let tracks = |found: Vec<FoundTrack>| found.into_iter().map(to_track).collect();
        Ok(match outcome {
            LoadOutcome::Track(resolved) => {
                let loaded = Loaded::from(resolved);
                self.keep(&loaded.track.encoded, &loaded);
                LoadResult::Track {
                    track: loaded.track,
                }
            }
            LoadOutcome::Described(found) => LoadResult::Track {
                track: to_track(found),
            },
            LoadOutcome::Playlist {
                playlist,
                tracks: found,
                ..
            } => LoadResult::Playlist {
                playlist,
                tracks: tracks(found),
            },
            LoadOutcome::Search(found) => LoadResult::Search {
                tracks: tracks(found),
            },
            LoadOutcome::Empty => LoadResult::Empty,
        })
    }

    pub async fn search(
        &self,
        ctx: &MediaContext,
        prefix: Option<&str>,
        terms: &str,
        limit: Option<u32>,
    ) -> Result<SearchResult, ApiError> {
        let prefix = prefix.unwrap_or(&ctx.config.sources.default_search);
        let limit = limit.unwrap_or(DEFAULT_SEARCH_RESULTS).max(1);
        let key = (prefix.to_owned(), terms.trim().to_lowercase(), limit);
        if let Some(cached) = self.searches.get(&key) {
            return Ok(cached);
        }
        let outcome = ctx
            .sources
            .search(Some(prefix), terms, limit as usize, ctx)
            .await
            .map_err(|e| load_error(&e))?;
        let result = SearchResult {
            prefix: outcome.prefix,
            provider: outcome.provider,
            tracks: outcome.tracks.into_iter().map(to_track).collect(),
        };
        self.searches.insert(key, result.clone());
        Ok(result)
    }

    /// An encoded track ready to play, from the cache or resolved again. Its info may be fuller
    /// than the encoded one, e.g. for a search result.
    pub async fn get(&self, ctx: &MediaContext, encoded: &str) -> Result<Loaded, ApiError> {
        if let Some(loaded) = self.resolved.get(encoded) {
            return Ok(loaded);
        }
        let found = decode(encoded)?;
        let loaded = Loaded::from(
            ctx.sources
                .resolve(&found, ctx)
                .await
                .map_err(|e| load_error(&e))?,
        );
        self.keep(encoded, &loaded);
        self.keep(&loaded.track.encoded, &loaded);
        Ok(loaded)
    }

    /// A query's track ready to play: a search's first result, or a playlist's track at `start_index`
    /// (else the one its link points at).
    pub async fn resolve_query(
        &self,
        ctx: &MediaContext,
        query: &str,
        start_index: Option<usize>,
    ) -> Result<Loaded, ApiError> {
        let loaded = Loaded::from(
            ctx.sources
                .resolve_query(query, start_index, ctx)
                .await
                .map_err(|e| load_error(&e))?,
        );
        self.keep(&loaded.track.encoded, &loaded);
        Ok(loaded)
    }

    /// Forgets a resolved track that failed to play, so that it is resolved afresh next time:
    /// its media URLs may no longer work, e.g. after the engine's address changed (YouTube's
    /// only work from the address that extracted them).
    pub fn forget(&self, encoded: &str) {
        self.resolved.remove(encoded);
    }

    /// Keeps a resolved track for playing soon, unless it is an upload: a kept upload would keep
    /// its file from expiring or being deleted, and finding one again costs nothing.
    pub(crate) fn keep(&self, encoded: &str, loaded: &Loaded) {
        if loaded.track.info.source_name == upload::SOURCE {
            return;
        }
        if let Some(keep_for) = reuse_for(loaded.valid_until, SystemTime::now()) {
            self.resolved
                .insert_for(encoded.to_owned(), loaded.clone(), keep_for);
        }
    }
}

/// How long a resolved track whose media URLs work `valid_until` is reused, from `now`: until
/// shortly before they stop, for at most [`KEEP_RESOLVED_FOR`]. `None` if that is too soon.
fn reuse_for(valid_until: Option<SystemTime>, now: SystemTime) -> Option<Duration> {
    let Some(until) = valid_until else {
        return Some(KEEP_UNDATED_FOR);
    };
    until
        .duration_since(now)
        .ok()?
        .checked_sub(URL_MARGIN)
        .filter(|left| !left.is_zero())
        .map(|left| left.min(KEEP_RESOLVED_FOR))
}

/// Attaches the client's data to a loaded track.
pub fn with_user_data(mut loaded: Loaded, user_data: Option<&Value>) -> Loaded {
    loaded.track.user_data = user_data.cloned();
    loaded
}

/// Maps a load failure to a stable error code.
pub fn load_error(e: &ResolveError) -> ApiError {
    let code = match e {
        ResolveError::SignInRequired { restriction, .. } => restriction_code(*restriction),
        ResolveError::Extraction { failure, .. } => match failure {
            Failure::Restricted(restriction) => restriction_code(*restriction),
            Failure::RateLimited => ErrorCode::RateLimited,
            Failure::Unavailable => ErrorCode::Unavailable,
            Failure::Other => ErrorCode::LoadFailed,
        },
        ResolveError::NoResults(_) | ResolveError::NoMirror(_) => ErrorCode::NoResults,
        ResolveError::PastEnd { .. } => ErrorCode::BadRequest,
        ResolveError::Spotify(error) => match error {
            SpotifyError::RateLimited(_) => ErrorCode::RateLimited,
            SpotifyError::NotFound(_) | SpotifyError::Restricted(_) => ErrorCode::Unavailable,
            SpotifyError::Credentials
            | SpotifyError::Http(_)
            | SpotifyError::Status { .. }
            | SpotifyError::Json(_) => ErrorCode::LoadFailed,
        },
        ResolveError::NoAudio(_) => ErrorCode::NoAudio,
        ResolveError::NoLocalFile(_) => ErrorCode::NotFound,
        ResolveError::Unsupported(_)
        | ResolveError::UnknownSearch(_)
        | ResolveError::UnknownSource(_)
        | ResolveError::PrivateAddress(_) => ErrorCode::BadRequest,
        ResolveError::HttpStatus { .. }
        | ResolveError::Lookup { .. }
        | ResolveError::UploadGone(_) => ErrorCode::Unavailable,
        ResolveError::YtDlpUnavailable(_)
        | ResolveError::Spawn(..)
        | ResolveError::Json(..)
        | ResolveError::Session(_)
        | ResolveError::Http { .. }
        | ResolveError::Probe { .. }
        | ResolveError::Timeout { .. } => ErrorCode::LoadFailed,
    };
    ApiError::new(code, display_chain(e))
}

fn restriction_code(restriction: Restriction) -> ErrorCode {
    match restriction {
        Restriction::AgeRestricted => ErrorCode::AgeRestricted,
        Restriction::BotCheck => ErrorCode::BotCheck,
        Restriction::MembersOnly => ErrorCode::MembersOnly,
        Restriction::Private => ErrorCode::Private,
        Restriction::SignIn => ErrorCode::SignInRequired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::protocol::Millis;

    fn found() -> FoundTrack {
        FoundTrack {
            reference: TrackRef {
                source: "youtube".into(),
                uri: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into(),
            },
            info: TrackInfo {
                identifier: "dQw4w9WgXcQ".into(),
                title: "Never Gonna Give You Up".into(),
                author: Some("Rick Astley".into()),
                author_url: None,
                album: None,
                length: Some(Millis(213_000)),
                is_stream: false,
                is_seekable: true,
                uri: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into(),
                artwork_url: None,
                isrc: None,
                source_name: "youtube".into(),
            },
        }
    }

    #[test]
    fn resolved_tracks_are_reused_while_their_urls_work() {
        let now = SystemTime::now();
        let hours = |h: u64| Duration::from_secs(h * 3600);
        assert_eq!(
            reuse_for(Some(now + hours(6)), now),
            Some(KEEP_RESOLVED_FOR),
            "YouTube's"
        );
        assert_eq!(
            reuse_for(Some(now + Duration::from_secs(125 * 60)), now),
            Some(Duration::from_secs(95 * 60)),
            "SoundCloud's"
        );
        assert_eq!(
            reuse_for(Some(now + Duration::from_secs(20 * 60)), now),
            None
        );
        assert_eq!(reuse_for(Some(now - hours(1)), now), None);
        assert_eq!(reuse_for(None, now), Some(KEEP_UNDATED_FOR));
    }

    #[test]
    fn encoding_round_trips_and_is_url_safe() {
        let encoded = encode(&found());
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(decode(&encoded).unwrap(), found());
        assert_eq!(
            decode("not a track").unwrap_err().code,
            ErrorCode::BadRequest
        );
    }
}
