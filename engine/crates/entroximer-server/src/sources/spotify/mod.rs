//! Spotify: links (tracks, albums, playlists, artists) and `spsearch:` for metadata, played from
//! YouTube.
//!
//! Spotify's audio is DRM-protected, so the provider only reads its catalog through the Web API
//! (with the client credentials of the operator's Spotify app) and plays each track from the
//! recording [`mirror`] finds on YouTube, as Lavalink's LavaSrc does. Tracks keep Spotify's
//! metadata: clients see the Spotify title, artists, artwork and link. Which recording a track
//! played from is remembered for a day, so repeats do not search again.
//!
//! Playlists are listed as Spotify's [`web`] player lists them, or failing that, as its
//! [`embed`]ded player does: the Web API only lists a playlist's tracks to its owner and
//! collaborators, never to an app's own credentials, unless the app has extended quota.

mod api;
mod embed;
mod mirror;
mod script;
mod web;

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde::de::DeserializeOwned;

pub use api::SpotifyError;
pub(crate) use mirror::DEFAULT_SEARCHES as DEFAULT_MIRROR_SEARCHES;
pub(crate) use web::DEFAULT_PLAYLIST_QUERY;

use self::api::{Image, Paging, SpotifyApi, Track};
use self::web::WebPlayer;
use super::provider::{MAX_SEARCH_RESULTS, Provider, ProviderDescriptor};
use super::{
    FoundTrack, Listing, LoadOutcome, PlaylistInfo, ResolveError, ResolvedTrack, TrackInfo,
    TrackRef, artwork,
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
    /// Lists playlists.
    web: WebPlayer,
    /// Whether Spotify withholds ISRCs from the app, as from apps in development mode; they are
    /// no longer looked up then.
    isrcs_withheld: AtomicBool,
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
            web: WebPlayer::new(&config.playlist_query),
            mirrors: Expiring::new(REMEMBER_MIRRORS_FOR, REMEMBERED_MIRRORS),
            isrcs_withheld: AtomicBool::new(false),
        })
    }

    /// Whether Spotify accepts the app's credentials.
    pub(super) async fn check(&self) -> Result<(), SpotifyError> {
        self.api.check().await
    }

    async fn load_album(
        &self,
        id: &str,
        listing: Listing,
        ctx: &MediaContext,
    ) -> Result<LoadOutcome, ResolveError> {
        let album = self.api.album(id).await?;
        let span = Span::new(listing, ctx);
        let artwork = largest(&album.images).map(str::to_owned);
        // an album's listing leaves out the album itself
        let tracks = self
            .page_through(album.tracks, span, |track: Track| {
                Ok(found_track(&track, Some((&album.name, artwork.as_deref()))))
            })
            .await?;
        let playlist = PlaylistInfo {
            name: album.name.clone(),
            author: artists(&album.artists),
            uri: spotify_url(album.external_urls.as_ref()),
            artwork_url: artwork,
            selected_track: None,
            total_tracks: album.total_tracks,
        };
        listed(playlist, tracks, span)
    }

    /// A playlist as Spotify's web player lists it; failing that, its first
    /// [`embed::MOST_ITEMS`] items, as the embedded player lists them.
    async fn load_playlist(
        &self,
        id: &str,
        listing: Listing,
        ctx: &MediaContext,
    ) -> Result<LoadOutcome, ResolveError> {
        let span = Span::new(listing, ctx);
        let fetch = |offset, limit| self.web.playlist(id, offset, limit);
        match page_through_web(id, span, fetch).await {
            Ok((info, tracks)) => listed(info, tracks, span),
            Err(e @ SpotifyError::NotFound(_)) => Err(e.into()),
            Err(e) => {
                tracing::warn!(
                    playlist = id,
                    "listing only the first {} tracks, as Spotify's embedded player does: {e}",
                    embed::MOST_ITEMS
                );
                load_embedded_playlist(id, span).await
            }
        }
    }

    /// The tracks of a paged listing from item `start` on, up to the span's limit; the listing
    /// jumps to the page holding the start.
    async fn page_through<T: DeserializeOwned>(
        &self,
        mut page: Paging<T>,
        span: Span,
        mut keep: impl FnMut(T) -> Result<Option<FoundTrack>, SpotifyError>,
    ) -> Result<Vec<FoundTrack>, SpotifyError> {
        // the offset of the page's first item
        let mut offset = 0;
        if span.start > 0 && span.start >= page.items.len() {
            let Some(next) = page.next.as_deref() else {
                return Ok(Vec::new());
            };
            page = self.api.page_at(next, span.start).await?;
            offset = span.start;
        }
        let mut tracks = Vec::new();
        loop {
            let skip = span.start.saturating_sub(offset);
            offset += page.items.len();
            for item in page.items.into_iter().skip(skip) {
                tracks.extend(keep(item)?);
            }
            if tracks.len() >= span.limit {
                tracks.truncate(span.limit);
                return Ok(tracks);
            }
            page = match self.api.next_page(page.next.as_deref()).await? {
                Some(page) => page,
                None => return Ok(tracks),
            };
        }
    }

    /// The artist's top tracks or, for apps without access to them, what a search for the
    /// artist finds.
    async fn load_artist(&self, id: &str, listing: Listing) -> Result<LoadOutcome, ResolveError> {
        let artist = self.api.artist(id).await?;
        let tracks = match self.api.top_tracks(id).await {
            Ok(top) => top,
            Err(SpotifyError::Restricted(_)) => {
                let query = format!("artist:\"{}\"", artist.name.replace('"', ""));
                self.api.search(&query, api::SEARCH_PAGE, 0).await?.items
            }
            Err(e) => return Err(e.into()),
        };
        let mut tracks: Vec<_> = tracks.iter().filter_map(|t| found_track(t, None)).collect();
        if let Listing::Queue {
            start_index: Some(start),
            ..
        } = listing
        {
            tracks.drain(..start.min(tracks.len()));
        }
        let playlist = PlaylistInfo {
            name: format!("{}: popular tracks", artist.name),
            author: Some(artist.name.clone()),
            uri: spotify_url(artist.external_urls.as_ref()),
            artwork_url: largest(&artist.images).map(str::to_owned),
            selected_track: None,
            total_tracks: None,
        };
        Ok(if tracks.is_empty() {
            LoadOutcome::Empty
        } else {
            LoadOutcome::Playlist { playlist, tracks }
        })
    }

    /// Notes, from a track Spotify described fully, whether it withholds ISRCs from the app: it
    /// leaves them out of every track for apps in development mode.
    fn note_isrcs(&self, full: &Track) {
        if full.external_ids.is_none() && !self.isrcs_withheld.swap(true, Ordering::Relaxed) {
            tracing::info!(
                "Spotify withholds ISRCs from the engine's app (it is in development mode), so \
                 tracks are found on YouTube by their artists and title only"
            );
        }
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
        // search results and listings come without the ISRC that finds a track best, as do
        // all tracks for apps in development mode, which are not asked again once that is known
        let mut wanted = track.info.clone();
        if wanted.isrc.is_none() && !self.isrcs_withheld.load(Ordering::Relaxed) {
            match self.api.track(id).await {
                Ok(full) => {
                    self.note_isrcs(&full);
                    wanted.isrc = full.external_ids.and_then(|ids| ids.isrc);
                }
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
        listing: Listing,
        ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError> {
        let Some(link) = Link::parse(url) else {
            return Ok(None);
        };
        let outcome = match link {
            Link::Track(id) => {
                let track = self.api.track(&id).await?;
                self.note_isrcs(&track);
                let found = found_track(&track, None)
                    .ok_or_else(|| ResolveError::NoAudio(url.to_string()))?;
                match listing {
                    // Spotify describes it fully; it is mirrored once it plays
                    Listing::Queue { .. } => LoadOutcome::Described(found),
                    Listing::Whole => LoadOutcome::Track(self.mirror(&found, ctx).await?),
                }
            }
            Link::Album(id) => self.load_album(&id, listing, ctx).await?,
            Link::Playlist(id) => self.load_playlist(&id, listing, ctx).await?,
            Link::Artist(id) => self.load_artist(&id, listing).await?,
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
        if !is_id(id) {
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

/// Whether `id` is what Spotify's IDs are, base62: they go into API paths.
fn is_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The ID in a URI such as `spotify:track:<id>`, for `kind` `track`.
fn uri_id<'a>(uri: &'a str, kind: &str) -> Option<&'a str> {
    let id = uri
        .strip_prefix("spotify:")?
        .strip_prefix(kind)?
        .strip_prefix(':')?;
    is_id(id).then_some(id)
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
        valid_until: mirror.valid_until,
    }
}

/// Where a listing starts and how far it goes.
#[derive(Clone, Copy, Debug)]
struct Span {
    start: usize,
    /// Most tracks listed.
    limit: usize,
}

impl Span {
    fn new(listing: Listing, ctx: &MediaContext) -> Self {
        let most = ctx.config.sources.playlist_limit.max(1) as usize;
        match listing {
            Listing::Whole => Self {
                start: 0,
                limit: most,
            },
            Listing::Queue { start_index, limit } => Self {
                start: start_index.unwrap_or(0),
                limit: limit.map_or(most, |limit| limit.clamp(1, most)),
            },
        }
    }
}

/// A collection's listing.
fn listed(
    playlist: PlaylistInfo,
    tracks: Vec<FoundTrack>,
    span: Span,
) -> Result<LoadOutcome, ResolveError> {
    if tracks.is_empty() {
        return match playlist.total_tracks {
            Some(total) if span.start > 0 && span.start >= total as usize => {
                Err(ResolveError::PastEnd { total })
            }
            _ => Ok(LoadOutcome::Empty),
        };
    }
    Ok(LoadOutcome::Playlist { playlist, tracks })
}

/// A playlist as Spotify's embedded player lists it: only its first [`embed::MOST_ITEMS`]
/// items, without albums (the playlist's picture stands in) or ISRCs.
async fn load_embedded_playlist(id: &str, span: Span) -> Result<LoadOutcome, ResolveError> {
    embedded_listing(id, embed::page(id).await?.playlist, span)
}

fn embedded_listing(
    id: &str,
    playlist: embed::Playlist,
    span: Span,
) -> Result<LoadOutcome, ResolveError> {
    let count = playlist.items.len();
    // a full page may have been cut short
    let complete = count < embed::MOST_ITEMS;
    if !complete {
        tracing::info!(
            playlist = %playlist.name,
            "Spotify's embedded player lists a playlist's first {count} tracks at most; this one may have more"
        );
        if span.start >= count {
            return Err(SpotifyError::Embed("it lists only a playlist's first 100 tracks").into());
        }
    }
    let mut tracks: Vec<FoundTrack> = playlist
        .items
        .into_iter()
        .skip(span.start)
        .flatten()
        .filter_map(|track| found_track(&track, None))
        .take(span.limit)
        .collect();
    let artwork = largest(&playlist.images).map(str::to_owned);
    artwork::inherit(&mut tracks, artwork.as_deref());
    let info = PlaylistInfo {
        name: playlist.name,
        author: playlist.owner,
        uri: Some(playlist_url(id)),
        artwork_url: artwork,
        selected_track: None,
        total_tracks: complete.then(|| u32::try_from(count).ok()).flatten(),
    };
    listed(info, tracks, span)
}

/// A playlist's tracks from the span's start on, up to its limit, with its details; `fetch(offset,
/// limit)` lists a page of its items as Spotify's web player does.
async fn page_through_web<F, Fut>(
    id: &str,
    span: Span,
    mut fetch: F,
) -> Result<(PlaylistInfo, Vec<FoundTrack>), SpotifyError>
where
    F: FnMut(usize, usize) -> Fut,
    Fut: Future<Output = Result<web::PlaylistPage, SpotifyError>>,
{
    let mut info = None;
    let mut tracks = Vec::new();
    let mut offset = span.start;
    loop {
        let wanted = (span.limit - tracks.len()).min(web::PAGE);
        let page = fetch(offset, wanted).await?;
        let count = page.items.len();
        offset += count;
        let total = page.total;
        info.get_or_insert_with(|| PlaylistInfo {
            name: page.name,
            author: page.owner,
            uri: Some(playlist_url(id)),
            artwork_url: largest(&page.images).map(str::to_owned),
            selected_track: None,
            total_tracks: total,
        });
        tracks.extend(
            page.items
                .into_iter()
                .flatten()
                .filter_map(|track| found_track(&track, None)),
        );
        let at_end = count == 0
            || match total {
                Some(total) => offset >= total as usize,
                None => count < wanted,
            };
        if at_end || tracks.len() >= span.limit {
            break;
        }
    }
    tracks.truncate(span.limit);
    let info = info.expect("a page was listed");
    artwork::inherit(&mut tracks, info.artwork_url.as_deref());
    Ok((info, tracks))
}

fn playlist_url(id: &str) -> String {
    format!("https://open.spotify.com/playlist/{id}")
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

    #[test]
    fn stops_looking_up_isrcs_spotify_withholds() {
        let provider = SpotifyProvider::new(&SpotifyConfig {
            client_id: Some("unused".into()),
            client_secret: Some(Secret::new("unused")),
            ..cli_defaults()
        })
        .unwrap();
        let mut track: Track =
            serde_json::from_str(include_str!("../../../tests/fixtures/spotify/track.json"))
                .unwrap();
        provider.note_isrcs(&track);
        assert!(
            !provider.isrcs_withheld.load(Ordering::Relaxed),
            "extended quota"
        );
        // as Spotify describes tracks to apps in development mode
        track.external_ids = None;
        provider.note_isrcs(&track);
        assert!(provider.isrcs_withheld.load(Ordering::Relaxed));
    }

    fn embedded_playlist() -> embed::Playlist {
        embed::parse(include_str!(
            "../../../tests/fixtures/spotify/embed_playlist.html"
        ))
        .unwrap()
        .playlist
    }

    #[test]
    fn lists_playlists_from_the_embedded_player() {
        let whole = Span {
            start: 0,
            limit: 1000,
        };
        let LoadOutcome::Playlist { playlist, tracks } =
            embedded_listing("37i9dQZF1DXcBWIGoYBM5M", embedded_playlist(), whole).unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.name, "Today’s Top Hits");
        assert_eq!(playlist.author.as_deref(), Some("Spotify"));
        assert_eq!(
            playlist.uri.as_deref(),
            Some("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M")
        );
        assert_eq!(playlist.total_tracks, Some(3), "the page lists them all");
        let titles: Vec<_> = tracks.iter().map(|t| t.info.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Stop The Wedding!", "expectations (feat. Chappell Roan)"],
            "the podcast episode is left out"
        );
        let track = &tracks[1].info;
        assert_eq!(
            track.author.as_deref(),
            Some("Olivia Rodrigo, Chappell Roan")
        );
        assert_eq!(track.length, Some(Millis(223_935)));
        assert_eq!(
            track.uri,
            "https://open.spotify.com/track/3XmwAZpIMNpTJy8faw3ajD"
        );
        assert_eq!(
            track.artwork_url, playlist.artwork_url,
            "the playlist's picture"
        );
        assert!(track.isrc.is_none());

        // items count from the playlist's first, the episode too
        let from_third = Span { start: 2, limit: 1 };
        let LoadOutcome::Playlist { tracks, .. } =
            embedded_listing("37i9dQZF1DXcBWIGoYBM5M", embedded_playlist(), from_third).unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].info.identifier, "3XmwAZpIMNpTJy8faw3ajD");

        let past_end = Span { start: 3, limit: 1 };
        assert!(matches!(
            embedded_listing("37i9dQZF1DXcBWIGoYBM5M", embedded_playlist(), past_end),
            Err(ResolveError::PastEnd { total: 3 })
        ));
    }

    #[test]
    fn long_embedded_playlists_are_cut_short() {
        let mut playlist = embedded_playlist();
        let track = playlist.items.swap_remove(0);
        let mut long = embed::Playlist {
            items: Vec::new(),
            ..playlist
        };
        long.items.resize_with(embed::MOST_ITEMS, || {
            track.as_ref().map(|t| api::Track {
                id: t.id.clone(),
                name: t.name.clone(),
                artists: Vec::new(),
                album: None,
                duration_ms: t.duration_ms,
                external_ids: None,
                is_local: false,
            })
        });
        let whole = Span {
            start: 0,
            limit: 1000,
        };
        let LoadOutcome::Playlist { playlist, tracks } =
            embedded_listing("37i9dQZF1DXcBWIGoYBM5M", long, whole).unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(tracks.len(), embed::MOST_ITEMS);
        assert_eq!(playlist.total_tracks, None, "there may be more");

        let past_page = Span {
            start: embed::MOST_ITEMS,
            limit: 1,
        };
        let long = embed::Playlist {
            items: (0..embed::MOST_ITEMS).map(|_| None).collect(),
            ..embedded_playlist()
        };
        assert!(matches!(
            embedded_listing("37i9dQZF1DXcBWIGoYBM5M", long, past_page),
            Err(ResolveError::Spotify(SpotifyError::Embed(_)))
        ));
    }

    /// Reads a playlist of Spotify's own from the real embedded player, which the Web API hides
    /// from apps. Needs the network, so it only runs when asked: `cargo test -- --ignored embed`.
    #[tokio::test]
    #[ignore = "reads open.spotify.com"]
    async fn reads_a_real_playlist_from_the_embedded_player() {
        let whole = Span {
            start: 0,
            limit: 1000,
        };
        // Today's Top Hits
        let LoadOutcome::Playlist { playlist, tracks } =
            load_embedded_playlist("37i9dQZF1DXcBWIGoYBM5M", whole)
                .await
                .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.author.as_deref(), Some("Spotify"));
        assert!(tracks.len() >= 10, "{} tracks", tracks.len());
        assert!(
            tracks
                .iter()
                .all(|t| t.info.author.is_some() && t.info.length.is_some())
        );

        let missing = load_embedded_playlist("0000000000000000000000", whole).await;
        assert!(
            matches!(
                missing,
                Err(ResolveError::Spotify(SpotifyError::NotFound(_)))
            ),
            "{missing:?}"
        );
    }

    #[test]
    fn reads_ids_from_uris() {
        assert_eq!(
            uri_id("spotify:track:4u7EnebtmKWzUH433cf5Qv", "track"),
            Some("4u7EnebtmKWzUH433cf5Qv")
        );
        assert_eq!(
            uri_id("spotify:episode:4u7EnebtmKWzUH433cf5Qv", "track"),
            None
        );
        assert_eq!(uri_id("spotify:trackx:abc", "track"), None);
        assert_eq!(uri_id("spotify:track:..%2Fme", "track"), None);
        assert_eq!(uri_id("spotify:track:", "track"), None);
    }

    /// Lists a playlist of `total` items, every tenth a podcast episode, as the web player would,
    /// with the pages asked for.
    async fn list_web(
        total: usize,
        span: Span,
    ) -> (PlaylistInfo, Vec<FoundTrack>, Vec<(usize, usize)>) {
        let item = |n: usize| {
            (n % 10 != 9).then(|| api::Track {
                id: Some(format!("track{n}")),
                name: format!("Track {n}"),
                artists: Vec::new(),
                album: None,
                duration_ms: 1000,
                external_ids: None,
                is_local: false,
            })
        };
        let mut asked = Vec::new();
        let (info, tracks) = page_through_web("abc", span, |offset, limit| {
            asked.push((offset, limit));
            std::future::ready(Ok(web::PlaylistPage {
                name: "Long".to_owned(),
                owner: Some("Someone".to_owned()),
                images: Vec::new(),
                total: Some(total as u32),
                items: (offset..total.min(offset + limit)).map(item).collect(),
            }))
        })
        .await
        .unwrap();
        (info, tracks, asked)
    }

    #[tokio::test]
    async fn pages_through_long_playlists() {
        let whole = Span {
            start: 0,
            limit: 1000,
        };
        let (info, tracks, asked) = list_web(250, whole).await;
        assert_eq!(asked, [(0, 100), (100, 100), (200, 100)]);
        assert_eq!(tracks.len(), 225, "without the episodes");
        assert_eq!(tracks[0].info.identifier, "track0");
        assert_eq!(tracks[224].info.identifier, "track248");
        assert_eq!(info.name, "Long");
        assert_eq!(info.total_tracks, Some(250));
        assert_eq!(
            info.uri.as_deref(),
            Some("https://open.spotify.com/playlist/abc")
        );

        // items count from the playlist's first, episodes too; a page asks for what is missing
        let (_, tracks, asked) = list_web(
            250,
            Span {
                start: 120,
                limit: 30,
            },
        )
        .await;
        assert_eq!(asked, [(120, 30), (150, 3)]);
        assert_eq!(tracks.len(), 30);
        assert_eq!(tracks[0].info.identifier, "track120");
        assert_eq!(tracks[29].info.identifier, "track152");

        let (info, tracks, asked) = list_web(
            50,
            Span {
                start: 60,
                limit: 10,
            },
        )
        .await;
        assert_eq!(asked, [(60, 10)]);
        assert!(tracks.is_empty());
        assert!(matches!(
            listed(
                info,
                tracks,
                Span {
                    start: 60,
                    limit: 10
                }
            ),
            Err(ResolveError::PastEnd { total: 50 })
        ));
    }

    /// Lists a playlist of Spotify's own past its first hundred tracks, with the real web player.
    /// Needs the network, so it only runs when asked: `cargo test -- --ignored web_player`.
    #[tokio::test]
    #[ignore = "reads open.spotify.com"]
    async fn lists_a_real_long_playlist_as_the_web_player_does() {
        let web = WebPlayer::new("");
        let whole = Span {
            start: 0,
            limit: 1000,
        };
        // Rock Classics
        let id = "37i9dQZF1DWXRqgorJj26U";
        let (info, tracks) =
            page_through_web(id, whole, |offset, limit| web.playlist(id, offset, limit))
                .await
                .unwrap();
        let total = info.total_tracks.unwrap() as usize;
        assert!(total > web::PAGE, "{total} tracks");
        assert!(
            tracks.len() > web::PAGE && tracks.len() <= total,
            "{} tracks",
            tracks.len()
        );
        assert!(tracks.iter().all(|t| t.info.album.is_some()
            && t.info.artwork_url.is_some()
            && t.info.author.is_some()
            && t.info.length.is_some()));
        assert_eq!(info.author.as_deref(), Some("Spotify"));

        let missing = web.playlist("0000000000000000000000", 0, 1).await;
        assert!(
            matches!(missing, Err(SpotifyError::NotFound(_))),
            "{missing:?}"
        );
    }
}
