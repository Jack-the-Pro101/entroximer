//! Playlists as Spotify's web player pages through them, which the Web API only lists to apps
//! with extended quota: its GraphQL API, with the anonymous token Spotify's [`super::embed`]ded
//! player hands every visitor. Unlike the embedded player's page, it lists a playlist from any
//! offset, and each track with its album and cover, though without its ISRC.
//!
//! The API is Spotify's own and undocumented: it only runs queries Spotify has registered, by the
//! hash of their text, and the hash of its playlist query changes when Spotify changes the query.
//! Once Spotify no longer runs the one the engine has, the engine reads the current one from the
//! web player's own [`super::script`]. A changed query may also take new variables: flags it
//! says are missing are sent as `false`.

use std::sync::{Mutex as SyncMutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::RETRY_AFTER;
use reqwest::{Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use super::super::net;
use super::api::{AlbumRef, ArtistRef, ExternalUrls, Image, SpotifyError, Track};
use super::embed::{self, CoverArt};
use super::script;
use crate::config::Secret;

/// The name of the web player's playlist query.
const PLAYLIST_QUERY: &str = "fetchPlaylist";
/// The hash of the web player's playlist query when this was written.
pub(crate) const DEFAULT_PLAYLIST_QUERY: &str =
    "21ea11a6ac75a5f3e01fd365d29c18e0a4a8b7dc09f89bbb5d73910190c6acaf";
/// The flags the playlist query took when this was written, all sent as `false`.
const PLAYLIST_QUERY_FLAGS: &[&str] = &["enableWatchFeedEntrypoint"];
/// The web player's script is read for the current query at most this often.
const LOOK_UP_AGAIN_AFTER: Duration = Duration::from_secs(3600);
/// Requests for one page at most: the query can be looked up, and a flag added, in between.
const ATTEMPTS: usize = 4;
const QUERY_URL: &str = "https://api-partner.spotify.com/pathfinder/v1/query";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Tokens are renewed this long before they expire.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
/// The most items asked for at once; a hundred tracks are about 120 KiB.
pub(super) const PAGE: usize = 100;

#[derive(Debug)]
pub(super) struct WebPlayer {
    query: SyncMutex<PlaylistQuery>,
    /// When the web player's script was last read for the query; held while it is read, so
    /// concurrent requests wait for one reading.
    looked_up: Mutex<Option<Instant>>,
    /// Held while it is renewed, so concurrent requests wait for one renewal.
    token: Mutex<Option<Token>>,
}

/// The playlist query as the engine runs it.
#[derive(Clone, Debug)]
struct PlaylistQuery {
    hash: String,
    /// Its variables that are flags, all sent as `false`.
    flags: Vec<String>,
}

/// What Spotify answered a query with.
#[derive(Debug)]
enum Answer {
    Page(PlaylistPage),
    /// It no longer runs a query by that hash.
    UnknownQuery,
    /// The query takes a flag it was not sent.
    MissingFlag(String),
}

#[derive(Debug)]
struct Token {
    value: Secret,
    renew_at: SystemTime,
}

/// Part of a playlist's items, with the playlist's details.
#[derive(Debug)]
pub(super) struct PlaylistPage {
    pub(super) name: String,
    pub(super) owner: Option<String>,
    pub(super) images: Vec<Image>,
    /// How many items the playlist has.
    pub(super) total: Option<u32>,
    /// `None` for podcast episodes, local files and tracks Spotify removed.
    pub(super) items: Vec<Option<Track>>,
}

impl WebPlayer {
    /// With the hash of the playlist query to start with; blank for the default.
    pub(super) fn new(playlist_query: &str) -> Self {
        let hash = match playlist_query.trim() {
            "" => DEFAULT_PLAYLIST_QUERY,
            hash => hash,
        };
        Self {
            query: SyncMutex::new(PlaylistQuery {
                hash: hash.to_owned(),
                flags: PLAYLIST_QUERY_FLAGS
                    .iter()
                    .map(|&flag| flag.to_owned())
                    .collect(),
            }),
            looked_up: Mutex::new(None),
            token: Mutex::new(None),
        }
    }

    /// Up to `limit` items of the playlist `id` (base62), from item `offset` on.
    pub(super) async fn playlist(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<PlaylistPage, SpotifyError> {
        let mut looked_up = false;
        for _ in 0..ATTEMPTS {
            let query = self.query().clone();
            match self.run(&query, id, offset, limit).await? {
                Answer::Page(page) => return Ok(page),
                Answer::UnknownQuery if !looked_up => {
                    looked_up = true;
                    self.look_up_query(&query.hash).await?;
                }
                Answer::UnknownQuery => {
                    return Err(SpotifyError::Web(
                        "Spotify does not run the web player's current playlist query either"
                            .to_owned(),
                    ));
                }
                Answer::MissingFlag(flag) => {
                    tracing::info!(
                        flag,
                        "Spotify's playlist query takes a new flag; sending it as false"
                    );
                    let mut current = self.query();
                    if !current.flags.contains(&flag) {
                        current.flags.push(flag);
                    }
                }
            }
        }
        Err(SpotifyError::Web(
            "Spotify keeps refusing the playlist query".to_owned(),
        ))
    }

    fn query(&self) -> MutexGuard<'_, PlaylistQuery> {
        self.query.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads the web player's script for the current playlist query, as Spotify no longer runs
    /// the one by the hash `stale`; at most once an hour.
    async fn look_up_query(&self, stale: &str) -> Result<(), SpotifyError> {
        let mut looked_up = self.looked_up.lock().await;
        // another request may have looked it up meanwhile
        if self.query().hash != stale {
            return Ok(());
        }
        if looked_up.is_some_and(|at| at.elapsed() < LOOK_UP_AGAIN_AFTER) {
            return Err(SpotifyError::Web(
                "Spotify no longer runs the playlist query, and the web player's script was read \
                 for the current one less than an hour ago"
                    .to_owned(),
            ));
        }
        *looked_up = Some(Instant::now());
        let hash = script::query_hash(PLAYLIST_QUERY).await.map_err(|e| {
            SpotifyError::Web(format!(
                "Spotify no longer runs the playlist query, and the current one cannot be read \
                 from the web player's script: {e}"
            ))
        })?;
        if hash == stale {
            return Err(SpotifyError::Web(
                "Spotify no longer runs the playlist query, though the web player still uses it"
                    .to_owned(),
            ));
        }
        tracing::info!(
            "Spotify changed the web player's playlist query; using its new one, which \
             ENTROXIMER_SPOTIFY_PLAYLIST_QUERY={hash} starts with"
        );
        self.query().hash = hash;
        Ok(())
    }

    /// Runs the playlist query, for up to `limit` items of the playlist `id` from item `offset` on.
    async fn run(
        &self,
        query: &PlaylistQuery,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Answer, SpotifyError> {
        let url = query_url(query, id, offset, limit);
        let mut response = self.send(&url, id, false).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            response = self.send(&url, id, true).await?;
        }
        match response.status() {
            StatusCode::UNAUTHORIZED => {
                return Err(SpotifyError::Web(
                    "Spotify refused the embedded player's token".to_owned(),
                ));
            }
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()?.parse().ok())
                    .unwrap_or(30);
                return Err(SpotifyError::RateLimited(retry_after));
            }
            _ => {}
        }
        let status = response.status();
        let body = response.bytes().await.map_err(SpotifyError::Http)?;
        match parse(&body) {
            // errors in the query's variables come with HTTP 400, other failures without them
            Err(SpotifyError::Json(_)) if !status.is_success() => {
                Err(SpotifyError::Web(format!("HTTP {status}")))
            }
            answer => answer,
        }
    }

    async fn send(&self, url: &Url, id: &str, renew_token: bool) -> Result<Response, SpotifyError> {
        let token = self.token(id, renew_token).await?;
        net::client()
            .get(url.clone())
            .bearer_auth(token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(SpotifyError::Http)
    }

    /// A current anonymous token, from the embedded player's page of the playlist `id` when it
    /// needs renewing; with `renew`, a fresh one even if the cached one looks valid.
    async fn token(&self, id: &str, renew: bool) -> Result<String, SpotifyError> {
        let mut token = self.token.lock().await;
        if let Some(current) = token
            .as_ref()
            .filter(|t| !renew && SystemTime::now() < t.renew_at)
        {
            return Ok(current.value.expose().to_owned());
        }
        let fresh = embed::page(id)
            .await?
            .token
            .ok_or(SpotifyError::Embed("the page has no token"))?;
        let value = fresh.value.expose().to_owned();
        *token = Some(Token {
            renew_at: fresh
                .expires_at
                .checked_sub(TOKEN_MARGIN)
                .unwrap_or(fresh.expires_at),
            value: fresh.value,
        });
        Ok(value)
    }
}

fn query_url(query: &PlaylistQuery, id: &str, offset: usize, limit: usize) -> Url {
    let mut variables = json!({
        "uri": format!("spotify:playlist:{id}"),
        "offset": offset,
        "limit": limit,
    });
    for flag in &query.flags {
        variables[flag] = false.into();
    }
    let extensions = json!({
        "persistedQuery": {"version": 1, "sha256Hash": query.hash},
    });
    let mut url = Url::parse(QUERY_URL).expect("the query URL is valid");
    url.query_pairs_mut()
        .append_pair("operationName", PLAYLIST_QUERY)
        .append_pair("variables", &variables.to_string())
        .append_pair("extensions", &extensions.to_string());
    url
}

fn parse(body: &[u8]) -> Result<Answer, SpotifyError> {
    let response: QueryResponse = serde_json::from_slice(body).map_err(SpotifyError::Json)?;
    if let Some(error) = response.errors.into_iter().next() {
        if error.message == "PersistedQueryNotFound" {
            return Ok(Answer::UnknownQuery);
        }
        if let Some(flag) = error.missing_flag() {
            return Ok(Answer::MissingFlag(flag));
        }
        return Err(SpotifyError::Web(error.message));
    }
    let playlist = match response.data.map(|data| data.playlist) {
        Some(PlaylistResult::Playlist(playlist)) => playlist,
        Some(PlaylistResult::Other) => return Err(SpotifyError::NotFound("playlist")),
        None => return Err(SpotifyError::Web("the answer has no data".to_owned())),
    };
    let playlist = *playlist;
    Ok(Answer::Page(PlaylistPage {
        name: playlist.name,
        owner: playlist
            .owner_v2
            .and_then(|owner| owner.data)
            .map(|owner| owner.name)
            .filter(|name| !name.is_empty()),
        images: playlist
            .images
            .and_then(|images| images.items.into_iter().next())
            .map(|art| art.sources)
            .unwrap_or_default(),
        total: playlist.content.total_count,
        items: playlist
            .content
            .items
            .into_iter()
            .map(|item| match item.item_v2 {
                Some(ItemResult::TrackResponseWrapper {
                    data: TrackResult::Track(track),
                }) => track.into_track(),
                _ => None,
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
struct QueryResponse {
    data: Option<QueryData>,
    #[serde(default)]
    errors: Vec<QueryError>,
}

#[derive(Debug, Deserialize)]
struct QueryError {
    message: String,
    extensions: Option<ErrorExtensions>,
}

#[derive(Debug, Deserialize)]
struct ErrorExtensions {
    /// The variable at fault, for errors in the variables.
    name: Option<String>,
    code: Option<String>,
}

impl QueryError {
    /// The flag the query was not sent, if that is what the error says: e.g. ``missing variable
    /// `$enableWatchFeedEntrypoint`: for required GraphQL type `Boolean!` ``.
    fn missing_flag(&self) -> Option<String> {
        let extensions = self.extensions.as_ref()?;
        let name = extensions.name.as_deref()?;
        let is_missing_flag = extensions.code.as_deref()
            == Some("VALIDATION_INVALID_TYPE_VARIABLE")
            && self.message.starts_with("missing variable")
            && self.message.ends_with("`Boolean!`");
        let is_name = !name.is_empty()
            && name.len() <= 64
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        (is_missing_flag && is_name).then(|| name.to_owned())
    }
}

#[derive(Debug, Deserialize)]
struct QueryData {
    #[serde(rename = "playlistV2")]
    playlist: PlaylistResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum PlaylistResult {
    Playlist(Box<PlaylistData>),
    /// `NotFound`, or whatever else Spotify answers instead.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistData {
    name: String,
    owner_v2: Option<Owner>,
    images: Option<Images>,
    content: Content,
}

#[derive(Debug, Deserialize)]
struct Owner {
    data: Option<Named>,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Images {
    #[serde(default)]
    items: Vec<CoverArt>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Content {
    total_count: Option<u32>,
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    item_v2: Option<ItemResult>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum ItemResult {
    TrackResponseWrapper {
        data: TrackResult,
    },
    /// Podcast episodes, local files.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum TrackResult {
    Track(TrackData),
    /// `NotFound` for tracks Spotify removed.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackData {
    /// `spotify:track:<id>`.
    uri: String,
    name: String,
    artists: Option<Artists>,
    album_of_track: Option<AlbumData>,
    track_duration: Option<TrackDuration>,
}

#[derive(Debug, Deserialize)]
struct Artists {
    #[serde(default)]
    items: Vec<ArtistData>,
}

#[derive(Debug, Deserialize)]
struct ArtistData {
    /// `spotify:artist:<id>`.
    uri: Option<String>,
    profile: Named,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumData {
    name: String,
    cover_art: Option<CoverArt>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackDuration {
    total_milliseconds: u64,
}

impl TrackData {
    /// `None` without a length, which matching the recording on YouTube needs.
    fn into_track(self) -> Option<Track> {
        let id = super::uri_id(&self.uri, "track")?;
        Some(Track {
            id: Some(id.to_owned()),
            name: self.name,
            artists: self
                .artists
                .map(|artists| artists.items)
                .unwrap_or_default()
                .into_iter()
                .map(|artist| ArtistRef {
                    external_urls: artist
                        .uri
                        .as_deref()
                        .and_then(|uri| super::uri_id(uri, "artist"))
                        .map(|id| ExternalUrls {
                            spotify: Some(format!("https://open.spotify.com/artist/{id}")),
                        }),
                    name: artist.profile.name,
                })
                .collect(),
            album: self.album_of_track.map(|album| AlbumRef {
                name: album.name,
                images: album.cover_art.map(|art| art.sources).unwrap_or_default(),
            }),
            duration_ms: self.track_duration?.total_milliseconds,
            external_ids: None,
            is_local: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_page_of_a_playlist() {
        let Answer::Page(page) = parse(include_bytes!(
            "../../../tests/fixtures/spotify/web_playlist.json"
        ))
        .unwrap() else {
            panic!("a page");
        };
        assert_eq!(page.name, "Rock Classics");
        assert_eq!(page.owner.as_deref(), Some("Spotify"));
        assert_eq!(page.total, Some(200));
        assert_eq!(
            page.images[0].url,
            "https://i.scdn.co/image/ab67706f00000002694bf33281695f3b7542a09a"
        );
        assert_eq!(page.items.len(), 4, "every item counts");
        assert!(page.items[1].is_none(), "a podcast episode");
        assert!(page.items[2].is_none(), "a track Spotify removed");

        let first = page.items[0].as_ref().unwrap();
        assert_eq!(first.id.as_deref(), Some("0ofHAoxe9vBkTCp2UQIavz"));
        assert_eq!(first.name, "Dreams - 2004 Remaster");
        assert_eq!(first.duration_ms, 257_800);
        let album = first.album.as_ref().unwrap();
        assert_eq!(album.name, "Rumours (Super Deluxe)");
        assert_eq!(album.images.len(), 3);
        let artist = &first.artists[0];
        assert_eq!(artist.name, "Fleetwood Mac");
        assert_eq!(
            artist.external_urls.as_ref().unwrap().spotify.as_deref(),
            Some("https://open.spotify.com/artist/08GQAI4eElDnROBrJRGE0X")
        );
        let artists: Vec<_> = page.items[3]
            .as_ref()
            .unwrap()
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect();
        assert_eq!(artists, ["Queen", "David Bowie"]);
    }

    #[test]
    fn missing_playlists_and_unknown_queries() {
        let missing = br#"{"data":{"playlistV2":{"__typename":"NotFound","message":"Object with uri 'spotify:playlist:0000000000000000000000' not found"}}}"#;
        assert!(matches!(
            parse(missing),
            Err(SpotifyError::NotFound("playlist"))
        ));
        let unknown = br#"{"errors":[{"message":"PersistedQueryNotFound"}]}"#;
        assert!(matches!(parse(unknown), Ok(Answer::UnknownQuery)));

        // as Spotify answered the query that replaced the default's predecessor
        let missing_flag = br#"{"errors":[{"message":"missing variable `$enableWatchFeedEntrypoint`: for required GraphQL type `Boolean!`","extensions":{"name":"enableWatchFeedEntrypoint","code":"VALIDATION_INVALID_TYPE_VARIABLE"}}]}"#;
        let Ok(Answer::MissingFlag(flag)) = parse(missing_flag) else {
            panic!("a missing flag");
        };
        assert_eq!(flag, "enableWatchFeedEntrypoint");

        let wrong_type = br#"{"errors":[{"message":"invalid variable `$enableWatchFeedEntrypoint`: found JSON string for GraphQL type `Boolean!`","extensions":{"name":"enableWatchFeedEntrypoint","code":"VALIDATION_INVALID_TYPE_VARIABLE"}}]}"#;
        assert!(matches!(parse(wrong_type), Err(SpotifyError::Web(_))));
        let missing_number = br#"{"errors":[{"message":"missing variable `$count`: for required GraphQL type `Int!`","extensions":{"name":"count","code":"VALIDATION_INVALID_TYPE_VARIABLE"}}]}"#;
        assert!(
            matches!(parse(missing_number), Err(SpotifyError::Web(_))),
            "only flags are guessed"
        );
    }

    #[test]
    fn sends_the_query_with_its_flags() {
        let player = WebPlayer::new(" ");
        let query = player.query().clone();
        assert_eq!(
            query.hash, DEFAULT_PLAYLIST_QUERY,
            "blank means the default"
        );
        assert_eq!(WebPlayer::new("abc").query().hash, "abc");

        let url = query_url(&query, "37i9dQZF1DWXRqgorJj26U", 100, 50);
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["operationName"], "fetchPlaylist");
        let variables: serde_json::Value = serde_json::from_str(&params["variables"]).unwrap();
        assert_eq!(
            variables,
            json!({
                "uri": "spotify:playlist:37i9dQZF1DWXRqgorJj26U",
                "offset": 100,
                "limit": 50,
                "enableWatchFeedEntrypoint": false,
            })
        );
        let extensions: serde_json::Value = serde_json::from_str(&params["extensions"]).unwrap();
        assert_eq!(
            extensions["persistedQuery"]["sha256Hash"],
            DEFAULT_PLAYLIST_QUERY
        );
    }

    /// Finds the current playlist query after starting with one Spotify does not run, with the
    /// real web player. Needs the network, so it only runs when asked:
    /// `cargo test -- --ignored web_player`.
    #[tokio::test]
    #[ignore = "reads open.spotify.com"]
    async fn looks_up_the_real_web_players_current_query() {
        let player = WebPlayer::new(&"0".repeat(64));
        // Rock Classics
        let page = player
            .playlist("37i9dQZF1DWXRqgorJj26U", 100, 5)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 5);
        assert_ne!(player.query().hash, "0".repeat(64));
        assert!(
            player.looked_up.lock().await.is_some(),
            "read from the script"
        );
    }
}
