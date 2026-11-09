//! Playlists as Spotify's embedded player lists them, when its [`super::web`] player's API fails.
//! (The Web API only lists playlists' tracks to apps with extended quota, and most apps do not
//! find Spotify's own playlists, such as Today's Top Hits, at all.)
//!
//! The player's page (`open.spotify.com/embed/playlist/<id>`) is public and carries the playlist
//! as JSON for its script. It lists the first [`MOST_ITEMS`] items, each track with its title,
//! artists and length, but not its album or ISRC. It also carries the anonymous token its player
//! uses, which the [`super::web`] player's API takes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::Deserialize;

use super::super::net;
use super::api::{ArtistRef, Image, SpotifyError, Track};
use crate::config::Secret;

const PAGE: &str = "https://open.spotify.com/embed/playlist/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A page listing a hundred tracks is about 150 KiB.
const MOST_BYTES: usize = 4 << 20;
/// The most items the page lists; longer playlists are cut short.
pub(super) const MOST_ITEMS: usize = 100;
/// Where the page's data starts, as Next.js writes it.
const DATA_START: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;
/// Separates the artists of a track.
const ARTIST_SEPARATOR: &str = ",\u{a0}";

/// What the embedded player's page of a playlist carries.
#[derive(Debug)]
pub(super) struct Page {
    pub(super) playlist: Playlist,
    pub(super) token: Option<AnonymousToken>,
}

/// A playlist as the embedded player lists it.
#[derive(Debug)]
pub(super) struct Playlist {
    pub(super) name: String,
    /// Who made it, e.g. `Spotify`.
    pub(super) owner: Option<String>,
    pub(super) images: Vec<Image>,
    /// Its first [`MOST_ITEMS`] items: `None` for podcast episodes and local files.
    pub(super) items: Vec<Option<Track>>,
}

/// The token the embedded player signs in with, as anyone who visits the page.
#[derive(Debug)]
pub(super) struct AnonymousToken {
    pub(super) value: Secret,
    pub(super) expires_at: SystemTime,
}

/// The page of the playlist `id` (base62, as [`super::Link`] checks).
pub(super) async fn page(id: &str) -> Result<Page, SpotifyError> {
    let mut response = net::client()
        .get(format!("{PAGE}{id}"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(SpotifyError::Http)?;
    match response.status() {
        StatusCode::NOT_FOUND => return Err(SpotifyError::NotFound("playlist")),
        StatusCode::TOO_MANY_REQUESTS => return Err(SpotifyError::RateLimited(30)),
        status if !status.is_success() => {
            return Err(SpotifyError::Status {
                status: status.as_u16(),
                message: "the embedded player's page is unavailable".to_owned(),
            });
        }
        _ => {}
    }
    let mut page = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(SpotifyError::Http)? {
        page.extend_from_slice(&chunk);
        if page.len() > MOST_BYTES {
            return Err(SpotifyError::Embed("the page is too large"));
        }
    }
    parse(&String::from_utf8_lossy(&page))
}

pub(super) fn parse(page: &str) -> Result<Page, SpotifyError> {
    let data = page
        .split_once(DATA_START)
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(data, _)| data)
        .ok_or(SpotifyError::Embed("the page has no data"))?;
    let data: Data = serde_json::from_str(data).map_err(SpotifyError::Json)?;
    // the page of a playlist that does not exist, or is private, only says so
    let state = data
        .props
        .page_props
        .state
        .ok_or(SpotifyError::NotFound("playlist"))?;
    let entity = state.data.entity;
    let token = state
        .settings
        .and_then(|settings| settings.session)
        .map(|session| AnonymousToken {
            value: Secret::new(session.access_token),
            expires_at: UNIX_EPOCH
                + Duration::from_millis(session.access_token_expiration_timestamp_ms),
        });
    Ok(Page {
        playlist: Playlist {
            name: entity.name,
            owner: entity.subtitle.filter(|owner| !owner.is_empty()),
            images: entity.cover_art.map(|art| art.sources).unwrap_or_default(),
            items: entity
                .track_list
                .into_iter()
                .map(Item::into_track)
                .collect(),
        },
        token,
    })
}

#[derive(Debug, Deserialize)]
struct Data {
    props: Props,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Props {
    page_props: PageProps,
}

#[derive(Debug, Deserialize)]
struct PageProps {
    /// Absent if there is no such playlist.
    state: Option<State>,
}

#[derive(Debug, Deserialize)]
struct State {
    data: StateData,
    settings: Option<Settings>,
}

#[derive(Debug, Deserialize)]
struct Settings {
    session: Option<Session>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    access_token: String,
    access_token_expiration_timestamp_ms: u64,
}

#[derive(Debug, Deserialize)]
struct StateData {
    entity: Entity,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entity {
    name: String,
    /// The owner's name.
    subtitle: Option<String>,
    cover_art: Option<CoverArt>,
    #[serde(default)]
    track_list: Vec<Item>,
}

/// A picture in its sizes.
#[derive(Debug, Deserialize)]
pub(super) struct CoverArt {
    #[serde(default)]
    pub(super) sources: Vec<Image>,
}

#[derive(Debug, Deserialize)]
struct Item {
    /// `spotify:track:<id>`, or an episode's or a local file's URI.
    uri: String,
    title: String,
    /// The artists, see [`ARTIST_SEPARATOR`].
    #[serde(default)]
    subtitle: String,
    /// In milliseconds.
    duration: u64,
}

impl Item {
    fn into_track(self) -> Option<Track> {
        let id = super::uri_id(&self.uri, "track")?;
        Some(Track {
            id: Some(id.to_owned()),
            name: self.title,
            artists: self
                .subtitle
                .split(ARTIST_SEPARATOR)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| ArtistRef {
                    name: name.to_owned(),
                    external_urls: None,
                })
                .collect(),
            album: None,
            duration_ms: self.duration,
            external_ids: None,
            is_local: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_playlist_and_token_from_the_page() {
        let page = parse(include_str!(
            "../../../tests/fixtures/spotify/embed_playlist.html"
        ))
        .unwrap();
        let token = page.token.unwrap();
        assert_eq!(token.value.expose(), "anonymous-token");
        assert_eq!(
            token.expires_at,
            UNIX_EPOCH + Duration::from_millis(1_794_119_269_227)
        );

        let playlist = page.playlist;
        assert_eq!(playlist.name, "Today’s Top Hits");
        assert_eq!(playlist.owner.as_deref(), Some("Spotify"));
        assert_eq!(
            playlist.images[0].url,
            "https://i.scdn.co/image/ab67706f00000002e06fabee7abffa117bed662f"
        );
        assert_eq!(playlist.items.len(), 3, "every item counts");
        assert!(playlist.items[1].is_none(), "a podcast episode");

        let first = playlist.items[0].as_ref().unwrap();
        assert_eq!(first.id.as_deref(), Some("3zOYmiNOuyDL0ClxJcEJ0T"));
        assert_eq!(first.name, "Stop The Wedding!");
        assert_eq!(first.duration_ms, 198_840);
        let artists: Vec<_> = playlist.items[2]
            .as_ref()
            .unwrap()
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect();
        assert_eq!(artists, ["Olivia Rodrigo", "Chappell Roan"]);
    }

    #[test]
    fn missing_playlists_and_other_pages() {
        let missing = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"status":404,"title":"Page not found"}},"page":"/playlist/[id]"}</script>"#;
        assert!(matches!(
            parse(missing),
            Err(SpotifyError::NotFound("playlist"))
        ));
        assert!(matches!(
            parse("<html><body>Something else</body></html>"),
            Err(SpotifyError::Embed(_))
        ));
    }

    #[test]
    fn leaves_out_local_files_and_odd_ids() {
        let item = |uri: &str| Item {
            uri: uri.to_owned(),
            title: "A song".to_owned(),
            subtitle: String::new(),
            duration: 1000,
        };
        assert!(
            item("spotify:local:Artist:Album:A+song:1")
                .into_track()
                .is_none()
        );
        assert!(item("spotify:track:..%2Fme").into_track().is_none());
        let track = item("spotify:track:4u7EnebtmKWzUH433cf5Qv")
            .into_track()
            .unwrap();
        assert!(track.artists.is_empty());
    }
}
