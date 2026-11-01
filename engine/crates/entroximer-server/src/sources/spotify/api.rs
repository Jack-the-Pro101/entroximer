//! The parts of Spotify's Web API the provider uses, signed in with the client credentials of
//! the operator's Spotify app. Only catalog data is read; nothing here plays audio.
//!
//! Since February 2026 Spotify limits apps in development mode: searches return at most ten
//! results per request, playlists only list their tracks to their owners (so never to client
//! credentials), and artists' top tracks are gone. Apps with extended quota keep everything.

use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Response, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use super::super::net;
use crate::config::Secret;

const API: &str = "https://api.spotify.com/v1";
const API_HOST: &str = "api.spotify.com";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Tokens are renewed this long before Spotify says they expire.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
/// The most results one search request returns to apps in development mode.
pub(super) const SEARCH_PAGE: usize = 10;

#[derive(Debug, thiserror::Error)]
pub enum SpotifyError {
    #[error("Spotify refused the engine's client ID and secret")]
    Credentials,
    #[error("Spotify is rate limiting the engine; try again in {0} s")]
    RateLimited(u64),
    #[error("Spotify has no such {0}")]
    NotFound(&'static str),
    #[error(
        "Spotify only lets apps with extended quota read {0}; this engine's app is in \
         development mode"
    )]
    Restricted(&'static str),
    #[error("cannot reach Spotify")]
    Http(#[source] reqwest::Error),
    #[error("Spotify answered HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("Spotify returned unexpected JSON")]
    Json(#[source] serde_json::Error),
}

#[derive(Debug)]
pub(super) struct SpotifyApi {
    client_id: String,
    client_secret: Secret,
    /// The country whose catalog is read.
    market: String,
    /// Held while it is renewed, so concurrent requests wait for one renewal.
    token: Mutex<Option<Token>>,
}

#[derive(Debug, Clone)]
struct Token {
    value: Secret,
    renew_at: Instant,
}

impl SpotifyApi {
    pub(super) fn new(client_id: String, client_secret: Secret, market: String) -> Self {
        Self {
            client_id,
            client_secret,
            market,
            token: Mutex::new(None),
        }
    }

    /// Signs in with the app's credentials, to find out early whether Spotify accepts them.
    pub(super) async fn check(&self) -> Result<(), SpotifyError> {
        self.token(true).await.map(drop)
    }

    pub(super) async fn track(&self, id: &str) -> Result<Track, SpotifyError> {
        self.get(&self.url(&format!("tracks/{id}"), &[]), "track")
            .await
    }

    pub(super) async fn album(&self, id: &str) -> Result<Album, SpotifyError> {
        self.get(&self.url(&format!("albums/{id}"), &[]), "album")
            .await
    }

    pub(super) async fn playlist(&self, id: &str) -> Result<Playlist, SpotifyError> {
        self.get(&self.url(&format!("playlists/{id}"), &[]), "playlist")
            .await
    }

    pub(super) async fn artist(&self, id: &str) -> Result<Artist, SpotifyError> {
        self.get(&self.url(&format!("artists/{id}"), &[]), "artist")
            .await
    }

    /// The artist's most popular tracks; refused to apps in development mode.
    pub(super) async fn top_tracks(&self, artist: &str) -> Result<Vec<Track>, SpotifyError> {
        let url = self.url(&format!("artists/{artist}/top-tracks"), &[]);
        match self.get::<TopTracks>(&url, "artist").await {
            Ok(top) => Ok(top.tracks),
            Err(SpotifyError::NotFound(_) | SpotifyError::Status { status: 403, .. }) => {
                Err(SpotifyError::Restricted("artists' top tracks"))
            }
            Err(e) => Err(e),
        }
    }

    /// One page of up to [`SEARCH_PAGE`] tracks matching `query`, starting at `offset`.
    pub(super) async fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Paging<Track>, SpotifyError> {
        let (limit, offset) = (limit.min(SEARCH_PAGE).to_string(), offset.to_string());
        let url = self.url(
            "search",
            &[
                ("q", query),
                ("type", "track"),
                ("limit", &limit),
                ("offset", &offset),
            ],
        );
        let found: SearchResults = self.get(&url, "search").await?;
        Ok(found.tracks)
    }

    /// The page a listing's `next` link points at, if it has one.
    pub(super) async fn next_page<T: DeserializeOwned>(
        &self,
        next: Option<&str>,
    ) -> Result<Option<Paging<T>>, SpotifyError> {
        let Some(next) = next else {
            return Ok(None);
        };
        let url = Url::parse(next).map_err(|_| SpotifyError::Status {
            status: 200,
            message: format!("invalid next page `{next}`"),
        })?;
        self.get(&url, "page").await.map(Some)
    }

    fn url(&self, path: &str, params: &[(&str, &str)]) -> Url {
        let mut url = Url::parse(&format!("{API}/{path}")).expect("API paths are valid");
        url.query_pairs_mut()
            .extend_pairs(params)
            .append_pair("market", &self.market);
        url
    }

    /// A GET request with the app's token, renewed once if Spotify no longer accepts it.
    async fn get<T: DeserializeOwned>(
        &self,
        url: &Url,
        what: &'static str,
    ) -> Result<T, SpotifyError> {
        // the token must only ever go to Spotify, whatever a response's `next` link says
        if url.scheme() != "https" || url.host_str() != Some(API_HOST) {
            return Err(SpotifyError::Status {
                status: 200,
                message: format!("refusing to follow `{url}` off the API"),
            });
        }
        let mut response = self.send(url, false).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            // tokens can end before their time
            response = self.send(url, true).await?;
        }
        match response.status() {
            StatusCode::UNAUTHORIZED => return Err(SpotifyError::Credentials),
            StatusCode::NOT_FOUND => return Err(SpotifyError::NotFound(what)),
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()?.parse().ok())
                    .unwrap_or(30);
                return Err(SpotifyError::RateLimited(retry_after));
            }
            status if !status.is_success() => {
                return Err(SpotifyError::Status {
                    status: status.as_u16(),
                    message: error_message(&response.bytes().await.unwrap_or_default()),
                });
            }
            _ => {}
        }
        let body = response.bytes().await.map_err(SpotifyError::Http)?;
        serde_json::from_slice(&body).map_err(SpotifyError::Json)
    }

    async fn send(&self, url: &Url, renew_token: bool) -> Result<Response, SpotifyError> {
        let token = self.token(renew_token).await?;
        net::client()
            .get(url.clone())
            .bearer_auth(token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(SpotifyError::Http)
    }

    /// A current access token; with `renew`, a fresh one even if the cached one looks valid.
    async fn token(&self, renew: bool) -> Result<String, SpotifyError> {
        let mut token = self.token.lock().await;
        if let Some(current) = token
            .as_ref()
            .filter(|t| !renew && Instant::now() < t.renew_at)
        {
            return Ok(current.value.expose().to_owned());
        }
        let response = net::client()
            .post(TOKEN_URL)
            .basic_auth(&self.client_id, Some(self.client_secret.expose()))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body("grant_type=client_credentials")
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(SpotifyError::Http)?;
        match response.status() {
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED => {
                return Err(SpotifyError::Credentials);
            }
            status if !status.is_success() => {
                return Err(SpotifyError::Status {
                    status: status.as_u16(),
                    message: error_message(&response.bytes().await.unwrap_or_default()),
                });
            }
            _ => {}
        }
        let body = response.bytes().await.map_err(SpotifyError::Http)?;
        let granted: TokenGrant = serde_json::from_slice(&body).map_err(SpotifyError::Json)?;
        let lifetime = Duration::from_secs(granted.expires_in).saturating_sub(TOKEN_MARGIN);
        *token = Some(Token {
            value: Secret::new(granted.access_token.clone()),
            renew_at: Instant::now() + lifetime,
        });
        Ok(granted.access_token)
    }
}

/// Spotify's own explanation in an error body, if it gave one.
fn error_message(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Body {
        error: Detail,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Detail {
        Message { message: String },
        Code(String),
    }
    match serde_json::from_slice::<Body>(body) {
        Ok(Body {
            error: Detail::Message { message } | Detail::Code(message),
        }) => message,
        Err(_) => String::from_utf8_lossy(body)
            .trim()
            .chars()
            .take(200)
            .collect(),
    }
}

#[derive(Debug, Deserialize)]
struct TokenGrant {
    access_token: String,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct Paging<T> {
    pub(super) items: Vec<T>,
    pub(super) next: Option<String>,
    pub(super) total: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SearchResults {
    tracks: Paging<Track>,
}

#[derive(Debug, Deserialize)]
struct TopTracks {
    tracks: Vec<Track>,
}

/// A track, or the simplified track of an album listing (no album, no ISRC).
#[derive(Debug, Deserialize)]
pub(super) struct Track {
    /// `None` for local files in playlists.
    pub(super) id: Option<String>,
    pub(super) name: String,
    #[serde(default)]
    pub(super) artists: Vec<ArtistRef>,
    pub(super) album: Option<AlbumRef>,
    pub(super) duration_ms: u64,
    pub(super) external_ids: Option<ExternalIds>,
    #[serde(default)]
    pub(super) is_local: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct ArtistRef {
    pub(super) name: String,
    pub(super) external_urls: Option<ExternalUrls>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AlbumRef {
    pub(super) name: String,
    #[serde(default)]
    pub(super) images: Vec<Image>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Album {
    pub(super) name: String,
    #[serde(default)]
    pub(super) artists: Vec<ArtistRef>,
    #[serde(default)]
    pub(super) images: Vec<Image>,
    pub(super) external_urls: Option<ExternalUrls>,
    pub(super) total_tracks: Option<u32>,
    pub(super) tracks: Paging<Track>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Playlist {
    pub(super) name: String,
    pub(super) owner: Option<Owner>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub(super) images: Vec<Image>,
    pub(super) external_urls: Option<ExternalUrls>,
    /// Absent unless the app may read the playlist's contents. Called `tracks` before the
    /// February 2026 changes, and still for apps with extended quota.
    #[serde(alias = "tracks")]
    pub(super) items: Option<Paging<PlaylistItem>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlaylistItem {
    /// A track or a podcast episode; `null` for items Spotify removed. Called `track` before
    /// the February 2026 changes.
    #[serde(alias = "track")]
    pub(super) item: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Owner {
    pub(super) display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Artist {
    pub(super) name: String,
    #[serde(default)]
    pub(super) images: Vec<Image>,
    pub(super) external_urls: Option<ExternalUrls>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Image {
    pub(super) url: String,
    pub(super) width: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ExternalIds {
    pub(super) isrc: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ExternalUrls {
    pub(super) spotify: Option<String>,
}

fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_errors_from_their_bodies() {
        assert_eq!(
            error_message(br#"{"error": {"status": 400, "message": "Invalid limit"}}"#),
            "Invalid limit"
        );
        assert_eq!(
            error_message(br#"{"error": "invalid_client", "error_description": "x"}"#),
            "invalid_client"
        );
        assert_eq!(error_message(b"  upstream down "), "upstream down");
    }

    #[tokio::test]
    async fn never_sends_the_token_off_the_api() {
        let api = SpotifyApi::new("id".into(), Secret::new("secret"), "US".into());
        let refused = api
            .next_page::<Track>(Some("https://evil.example/v1/next"))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("refusing"), "{refused}");
    }

    #[test]
    fn reads_playlists_before_and_after_the_2026_changes() {
        let current: Playlist = serde_json::from_str(
            r#"{"name": "Mine", "images": null, "items": {"items": [{"item": {"id": "a"}}], "next": null}}"#,
        )
        .unwrap();
        assert!(current.images.is_empty());
        assert!(current.items.unwrap().items[0].item.is_some());

        let legacy: Playlist = serde_json::from_str(
            r#"{"name": "Theirs", "tracks": {"items": [{"track": null}], "next": null}}"#,
        )
        .unwrap();
        assert!(legacy.items.unwrap().items[0].item.is_none());

        let restricted: Playlist = serde_json::from_str(r#"{"name": "Other"}"#).unwrap();
        assert!(restricted.items.is_none());
    }
}
