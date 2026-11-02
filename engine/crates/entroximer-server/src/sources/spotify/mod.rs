//! Spotify: links (tracks, albums, playlists, artists) and `spsearch:` for metadata, played from
//! YouTube.
//!
//! Spotify's audio is DRM-protected, so the provider only reads its catalog through the Web API
//! (with the client credentials of the operator's Spotify app) and plays each track from the
//! recording [`mirror`] finds on YouTube, as Lavalink's LavaSrc does. Tracks keep Spotify's
//! metadata: clients see the Spotify title, artists, artwork and link. Which recording a track
//! played from is remembered for a day, so repeats do not search again.

mod api;
mod mirror;

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;

pub use api::SpotifyError;
pub(crate) use mirror::DEFAULT_SEARCHES as DEFAULT_MIRROR_SEARCHES;

use self::api::{Image, Paging, SpotifyApi, Track};
use super::provider::{MAX_SEARCH_RESULTS, Provider, ProviderDescriptor};
use super::{
    FoundTrack, LoadOutcome, PlaylistInfo, ResolveError, ResolvedTrack, TrackInfo, TrackRef,
    artwork,
};
use crate::api::protocol::Millis;
use crate::config::{MediaContext, SpotifyConfig};
use crate::expiring::Expiring;

const NAME: &str = "spotify";
const SEARCH_PREFIX: &str = "spsearch";
const HOSTS: &[&str] = &["open.spotify.com", "play.spotify.com"];
const REMEMBER_MIRRORS_FOR: Duration = Duration::from_secs(24 * 3600);
const REMEMBERED_MIRRORS: usize = 4096;
/// Mirror results tried before the next mirror search, when the best ones fail to load.
const MIRROR_ATTEMPTS: usize = 2;

pub(super) struct SpotifyProvider {
    descriptor: ProviderDescriptor,
    api: SpotifyApi,
    /// Mirror searches, e.g. `ytsearch:"{isrc}"`; see [`mirror::fill`].
    mirror_searches: Vec<String>,
    /// The recording each Spotify track (by ID) played from.
    mirrors: Expiring<String, FoundTrack>,
}

impl SpotifyProvider {
    /// `None` without credentials: Spotify's API needs an app. Empty ones count as none, as
    /// compose files pass unset variables.
    pub(super) fn new(config: &SpotifyConfig) -> Option<Self> {
        let id = config.client_id.clone().filter(|id| !id.trim().is_empty());
        let secret = config
            .client_secret
            .clone()
            .filter(|secret| !secret.is_blank());
        let (Some(id), Some(secret)) = (id, secret) else {
            return None;
        };
        Some(Self {
            descriptor: ProviderDescriptor {
                name: NAME.to_owned(),
                display_name: "Spotify (plays from YouTube)".to_owned(),
                search_prefixes: vec![SEARCH_PREFIX.to_owned()],
                hosts: HOSTS.iter().map(|&host| host.to_owned()).collect(),
            },
            api: SpotifyApi::new(id, secret, config.market.clone()),
            mirror_searches: config.mirror.clone(),
            mirrors: Expiring::new(REMEMBER_MIRRORS_FOR, REMEMBERED_MIRRORS),
        })
    }

    /// Whether Spotify accepts the app's credentials.
    pub(super) async fn check(&self) -> Result<(), SpotifyError> {
        self.api.check().await
    }

    async fn load_album(&self, id: &str, ctx: &MediaContext) -> Result<LoadOutcome, ResolveError> {
        let album = self.api.album(id).await?;
        let limit = ctx.config.sources.playlist_limit as usize;
        let mut tracks = Vec::new();
        let mut page = Some(album.tracks);
        let artwork = largest(&album.images);
        while let Some(Paging { items, next, .. }) = page {
            // an album's listing leaves out the album itself
            tracks.extend(
                items
                    .iter()
                    .filter_map(|track| found_track(track, Some((&album.name, artwork)))),
            );
            page = if tracks.len() < limit {
                self.api.next_page(next.as_deref()).await?
            } else {
                None
            };
        }
        tracks.truncate(limit);
        let playlist = PlaylistInfo {
            name: album.name.clone(),
            author: artists(&album.artists),
            uri: spotify_url(album.external_urls.as_ref()),
            artwork_url: artwork.map(str::to_owned),
            selected_track: None,
            total_tracks: album.total_tracks,
        };
        Ok(listing(playlist, tracks))
    }

    async fn load_playlist(
        &self,
        id: &str,
        ctx: &MediaContext,
    ) -> Result<LoadOutcome, ResolveError> {
        let playlist = self.api.playlist(id).await?;
        let Some(first) = playlist.items else {
            return Err(
                SpotifyError::Restricted("the tracks of playlists their users do not own").into(),
            );
        };
        let limit = ctx.config.sources.playlist_limit as usize;
        let total = first.total;
        let mut tracks = Vec::new();
        let mut page = Some(first);
        while let Some(Paging { items, next, .. }) = page {
            for item in items.into_iter().filter_map(|i| i.item) {
                // podcast episodes and removed tracks are left out
                if item.get("type").and_then(|t| t.as_str()) != Some("track") {
                    continue;
                }
                let track: Track = serde_json::from_value(item).map_err(SpotifyError::Json)?;
                tracks.extend(found_track(&track, None));
            }
            page = if tracks.len() < limit {
                self.api.next_page(next.as_deref()).await?
            } else {
                None
            };
        }
        tracks.truncate(limit);
        let artwork = largest(&playlist.images).map(str::to_owned);
        artwork::inherit(&mut tracks, artwork.as_deref());
        let info = PlaylistInfo {
            name: playlist.name,
            author: playlist.owner.and_then(|owner| owner.display_name),
            uri: spotify_url(playlist.external_urls.as_ref()),
            artwork_url: artwork,
            selected_track: None,
            total_tracks: total,
        };
        Ok(listing(info, tracks))
    }

    /// The artist's top tracks or, for apps without access to them, what a search for the
    /// artist finds.
    async fn load_artist(&self, id: &str) -> Result<LoadOutcome, ResolveError> {
        let artist = self.api.artist(id).await?;
        let tracks = match self.api.top_tracks(id).await {
            Ok(top) => top,
            Err(SpotifyError::Restricted(_)) => {
                let query = format!("artist:\"{}\"", artist.name.replace('"', ""));
                self.api.search(&query, api::SEARCH_PAGE, 0).await?.items
            }
            Err(e) => return Err(e.into()),
        };
        let tracks = tracks.iter().filter_map(|t| found_track(t, None)).collect();
        let playlist = PlaylistInfo {
            name: format!("{}: popular tracks", artist.name),
            author: Some(artist.name.clone()),
            uri: spotify_url(artist.external_urls.as_ref()),
            artwork_url: largest(&artist.images).map(str::to_owned),
            selected_track: None,
            total_tracks: None,
        };
        Ok(listing(playlist, tracks))
    }

    /// Plays `track` from its recording on YouTube, keeping Spotify's metadata.
    async fn mirror(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let id = track.info.identifier.as_str();
        if let Some(known) = self.mirrors.get(id) {
            match ctx.sources.resolve(&known, ctx).await {
                Ok(resolved) => return Ok(as_spotify(track, resolved)),
                Err(e) => tracing::debug!(
                    track = %track.info.title,
                    "the recording played before is gone, searching again: {e}"
                ),
            }
        }
        // search results and album listings come without the ISRC that finds a track best
        let mut wanted = track.info.clone();
        if wanted.isrc.is_none() {
            match self.api.track(id).await {
                Ok(full) => wanted.isrc = full.external_ids.and_then(|ids| ids.isrc),
                Err(e) => tracing::debug!(track = %wanted.title, "cannot look up the ISRC: {e}"),
            }
        }

        let mut last_error = None;
        for template in &self.mirror_searches {
            let Some(search) = mirror::fill(template, &wanted) else {
                continue;
            };
            let Some((prefix, terms)) = search.split_once(':') else {
                tracing::warn!(
                    template,
                    "a Spotify mirror search needs a prefix, as in `ytsearch:`"
                );
                continue;
            };
            let found = match ctx
                .sources
                .search(Some(prefix), terms, mirror::CANDIDATES, ctx)
                .await
            {
                Ok(found) => found.tracks,
                Err(e) => {
                    tracing::debug!(%search, "mirror search failed: {e}");
                    last_error = Some(e);
                    continue;
                }
            };
            let mut candidates: Vec<(f32, FoundTrack)> = found
                .into_iter()
                // never another Spotify track, which would mirror itself
                .filter(|candidate| candidate.reference.source != NAME)
                .filter_map(|candidate| {
                    mirror::score(&wanted, &candidate.info).map(|score| (score, candidate))
                })
                .collect();
            // stable, so equal scores keep the search's order
            candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, candidate) in candidates.into_iter().take(MIRROR_ATTEMPTS) {
                match ctx.sources.resolve(&candidate, ctx).await {
                    Ok(resolved) => {
                        tracing::debug!(
                            track = %wanted.title,
                            mirror = %candidate.reference.uri,
                            %search,
                            "found the recording on YouTube"
                        );
                        self.mirrors.insert(id.to_owned(), candidate);
                        return Ok(as_spotify(track, resolved));
                    }
                    Err(e) => last_error = Some(e),
                }
            }
        }
        Err(last_error.unwrap_or_else(|| ResolveError::NoMirror(describe(&wanted))))
    }
}

impl std::fmt::Debug for SpotifyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpotifyProvider")
            .field("mirror_searches", &self.mirror_searches)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for SpotifyProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn accepts(&self, url: &Url) -> bool {
        Link::parse(url).is_some()
    }

    async fn load_url(
        &self,
        url: &Url,
        ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError> {
        let Some(link) = Link::parse(url) else {
            return Ok(None);
        };
        let outcome = match link {
            Link::Track(id) => {
                let track = self.api.track(&id).await?;
                let found = found_track(&track, None)
                    .ok_or_else(|| ResolveError::NoAudio(url.to_string()))?;
                LoadOutcome::Track(self.mirror(&found, ctx).await?)
            }
            Link::Album(id) => self.load_album(&id, ctx).await?,
            Link::Playlist(id) => self.load_playlist(&id, ctx).await?,
            Link::Artist(id) => self.load_artist(&id).await?,
        };
        Ok(Some(outcome))
    }

    async fn search(
        &self,
        prefix: &str,
        terms: &str,
        limit: usize,
        _ctx: &MediaContext,
    ) -> Result<Vec<FoundTrack>, ResolveError> {
        if prefix != SEARCH_PREFIX {
            return Err(ResolveError::UnknownSearch(prefix.to_owned()));
        }
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS);
        let mut tracks = Vec::new();
        // counted in Spotify's items, some of which (local files) are left out
        let mut offset = 0;
        while tracks.len() < limit {
            let page = self.api.search(terms, limit - tracks.len(), offset).await?;
            offset += page.items.len();
            let more = page.next.is_some() && page.items.len() == api::SEARCH_PAGE;
            tracks.extend(page.items.iter().filter_map(|t| found_track(t, None)));
            if !more {
                break;
            }
        }
        tracks.truncate(limit);
        Ok(tracks)
    }

    async fn resolve(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        self.mirror(track, ctx).await
    }
}

/// What a Spotify link points at, by ID.
#[derive(Debug, PartialEq, Eq)]
enum Link {
    Track(String),
    Album(String),
    Playlist(String),
    Artist(String),
}

impl Link {
    /// `https://open.spotify.com/track/<id>` (also with an `intl-xx/` or `embed/` segment
    /// first) or `spotify:track:<id>`, likewise for albums, playlists and artists.
    fn parse(url: &Url) -> Option<Self> {
        let (kind, id) = match url.scheme() {
            "spotify" => url.path().split_once(':')?,
            "http" | "https" if url.host_str().is_some_and(|host| HOSTS.contains(&host)) => {
                let mut segments = url
                    .path_segments()?
                    .filter(|s| !s.is_empty())
                    .skip_while(|s| s.starts_with("intl-") || *s == "embed");
                (segments.next()?, segments.next()?)
            }
            _ => return None,
        };
        // IDs go into API paths, so they must be what Spotify's are: base62
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let id = id.to_owned();
        Some(match kind {
            "track" => Self::Track(id),
            "album" => Self::Album(id),
            "playlist" => Self::Playlist(id),
            "artist" => Self::Artist(id),
            _ => return None,
        })
    }
}

/// A Spotify track in the common model; `album` names (and pictures) the album of an album's
/// listing, whose tracks leave it out. `None` for local files, which have no ID.
fn found_track(track: &Track, album: Option<(&str, Option<&str>)>) -> Option<FoundTrack> {
    if track.is_local {
        return None;
    }
    let id = track.id.clone()?;
    let uri = format!("https://open.spotify.com/track/{id}");
    let (album_name, artwork) = match (&track.album, album) {
        (Some(own), _) => (
            Some(own.name.clone()),
            largest(&own.images).map(str::to_owned),
        ),
        (None, Some((name, artwork))) => (Some(name.to_owned()), artwork.map(str::to_owned)),
        (None, None) => (None, None),
    };
    Some(FoundTrack {
        reference: TrackRef {
            source: NAME.to_owned(),
            uri: uri.clone(),
        },
        info: TrackInfo {
            identifier: id,
            title: track.name.clone(),
            author: artists(&track.artists),
            author_url: track
                .artists
                .first()
                .and_then(|artist| spotify_url(artist.external_urls.as_ref())),
            album: album_name,
            length: Some(Millis(track.duration_ms)),
            is_stream: false,
            is_seekable: true,
            uri,
            artwork_url: artwork,
            isrc: track.external_ids.as_ref().and_then(|ids| ids.isrc.clone()),
            source_name: NAME.to_owned(),
        },
    })
}

/// A Spotify track playing from its mirror: Spotify's metadata, with the length of what
/// actually plays.
fn as_spotify(track: &FoundTrack, mirror: ResolvedTrack) -> ResolvedTrack {
    let mut info = track.info.clone();
    info.length = mirror.track.info.length.or(info.length);
    ResolvedTrack {
        track: FoundTrack {
            reference: track.reference.clone(),
            info,
        },
        producer: mirror.producer,
    }
}

fn listing(playlist: PlaylistInfo, tracks: Vec<FoundTrack>) -> LoadOutcome {
    if tracks.is_empty() {
        LoadOutcome::Empty
    } else {
        LoadOutcome::Playlist { playlist, tracks }
    }
}

fn artists(artists: &[api::ArtistRef]) -> Option<String> {
    let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
    (!names.is_empty()).then(|| names.join(", "))
}

fn spotify_url(urls: Option<&api::ExternalUrls>) -> Option<String> {
    urls.and_then(|urls| urls.spotify.clone())
}

fn largest(images: &[Image]) -> Option<&str> {
    images
        .iter()
        .max_by_key(|image| image.width.unwrap_or(0))
        .map(|image| image.url.as_str())
}

/// `Artist - Title`, for messages.
fn describe(track: &TrackInfo) -> String {
    match &track.author {
        Some(author) => format!("{author} - {}", track.title),
        None => track.title.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;
    use crate::sources::spotify::api::Album;

    fn link(text: &str) -> Option<Link> {
        Link::parse(&Url::parse(text).unwrap())
    }

    #[test]
    fn parses_links_and_uris() {
        assert_eq!(
            link("https://open.spotify.com/track/4u7EnebtmKWzUH433cf5Qv?si=abc"),
            Some(Link::Track("4u7EnebtmKWzUH433cf5Qv".into()))
        );
        assert_eq!(
            link("https://open.spotify.com/intl-de/album/1xn54DMo2qIqBuMqHtUsFd"),
            Some(Link::Album("1xn54DMo2qIqBuMqHtUsFd".into()))
        );
        assert_eq!(
            link("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M"),
            Some(Link::Playlist("37i9dQZF1DXcBWIGoYBM5M".into()))
        );
        assert_eq!(
            link("spotify:artist:4gzpq5DPGxSnKTe4SA8HAU"),
            Some(Link::Artist("4gzpq5DPGxSnKTe4SA8HAU".into()))
        );
        assert_eq!(link("https://open.spotify.com/episode/abc"), None);
        assert_eq!(
            link("https://open.spotify.com/track/..%2Fme"),
            None,
            "IDs are base62"
        );
        assert_eq!(link("https://notspotify.com/track/abc"), None);
    }

    #[test]
    fn tracks_carry_spotify_metadata() {
        let track: Track =
            serde_json::from_str(include_str!("../../../tests/fixtures/spotify/track.json"))
                .unwrap();
        let found = found_track(&track, None).unwrap();
        assert_eq!(found.reference.source, "spotify");
        assert_eq!(found.info.title, "Viva La Vida");
        assert_eq!(found.info.author.as_deref(), Some("Coldplay"));
        assert_eq!(
            found.info.album.as_deref(),
            Some("Viva La Vida or Death and All His Friends")
        );
        assert_eq!(found.info.isrc.as_deref(), Some("GBAYE0800265"));
        assert_eq!(found.info.length, Some(Millis(242_373)));
        assert_eq!(
            found.info.artwork_url.as_deref(),
            Some("https://i.scdn.co/image/ab67616d0000b273e21cc1db05580b6f2d2a3b6e"),
            "the largest picture"
        );
        assert_eq!(
            found.info.author_url.as_deref(),
            Some("https://open.spotify.com/artist/4gzpq5DPGxSnKTe4SA8HAU")
        );
    }

    /// Finds a well-known song on YouTube through the real mirror searches. Needs the network
    /// and yt-dlp, so it only runs when asked: `cargo test -- --ignored mirrors`.
    #[tokio::test]
    #[ignore = "searches YouTube"]
    async fn mirrors_a_known_track_on_youtube() {
        use clap::Parser;

        use crate::config::MediaConfig;

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            media: MediaConfig,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let cli = Cli::parse_from([
            "entroximer",
            "--data-dir",
            &path("data"),
            "--cache-dir",
            &path("cache"),
        ]);
        let ctx = MediaContext::new(cli.media).unwrap();
        let provider = SpotifyProvider::new(&SpotifyConfig {
            client_id: Some("unused".into()),
            client_secret: Some(Secret::new("unused")),
            ..cli_defaults()
        })
        .unwrap();
        let track: Track =
            serde_json::from_str(include_str!("../../../tests/fixtures/spotify/track.json"))
                .unwrap();
        // with its ISRC known, mirroring asks Spotify nothing
        let found = found_track(&track, None).unwrap();
        let resolved = provider.mirror(&found, &ctx).await.unwrap();
        assert_eq!(
            resolved.track.info.title, "Viva La Vida",
            "Spotify's metadata stays"
        );
        let mirror = provider
            .mirrors
            .get(found.info.identifier.as_str())
            .unwrap();
        assert_eq!(mirror.reference.source, "youtube");
        let length = resolved.track.info.length.unwrap().0;
        assert!(length.abs_diff(242_373) < 7_000, "played {length} ms");
    }

    fn cli_defaults() -> SpotifyConfig {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            spotify: SpotifyConfig,
        }
        Cli::parse_from(["entroximer"]).spotify
    }

    #[test]
    fn album_listings_get_the_album_and_local_files_are_skipped() {
        let album: Album =
            serde_json::from_str(include_str!("../../../tests/fixtures/spotify/album.json"))
                .unwrap();
        let artwork = largest(&album.images);
        let tracks: Vec<_> = album
            .tracks
            .items
            .iter()
            .filter_map(|t| found_track(t, Some((&album.name, artwork))))
            .collect();
        assert_eq!(tracks.len(), 2);
        assert!(
            tracks
                .iter()
                .all(|t| t.info.album.as_deref() == Some(album.name.as_str()))
        );
        assert!(
            tracks
                .iter()
                .all(|t| t.info.artwork_url.is_some() && t.info.isrc.is_none())
        );

        let local: Track = serde_json::from_str(
            r#"{"id": null, "name": "My file", "duration_ms": 1000, "is_local": true}"#,
        )
        .unwrap();
        assert!(found_track(&local, None).is_none());
    }
}
