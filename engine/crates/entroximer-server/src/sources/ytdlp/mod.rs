//! Providers backed by yt-dlp: the named sites of [`profiles`] (YouTube, SoundCloud, ...) and
//! any other site yt-dlp supports.
//!
//! Links are loaded with `yt-dlp -J --flat-playlist`: a playlist comes back as a list of entries
//! with metadata only, a single track fully extracted, ready to play. For a queue, a playlist is
//! listed from its start track (see [`Listing::Queue`]). Searches list results the same way. A
//! track is fully extracted when it is resolved for playback, and the resulting info JSON is fed
//! back with `--load-info-json -` for every download, so a page is never extracted twice. The chosen audio format's protocol decides how its bytes arrive:
//!
//! - `https`: yt-dlp writes the file to stdout. Re-opening at an offset goes through ffmpeg,
//!   which seeks the format's direct URL with range requests using the container index.
//! - `sabr`: YouTube's server-driven streaming (yt-dlp PR #13515) cannot be written to stdout,
//!   so yt-dlp writes files that are read while they grow. Seeking waits for the download. Its
//!   downloads extract the page again instead of reading the info JSON: mid-stream, the SABR
//!   downloader fetches fresh PO tokens and reloads the stream through the extractor, which a
//!   loaded info JSON cannot provide.
//! - `m3u8`/`m3u8_native` (SoundCloud, live streams): ffmpeg plays the playlist, seeking by
//!   segment unless it is live; yt-dlp's own stdout output would be MPEG-TS, which the demuxer
//!   does not read.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use entroximer_core::error::display_chain;
use entroximer_core::producer::ffmpeg::FfmpegProducer;
use entroximer_core::producer::growing_file::GrowingFileProducer;
use entroximer_core::producer::process::{ProcessProducer, ProcessSpec};
use entroximer_core::producer::{Producer, ProducerCaps, ProducerStream, TimestampMode};
use entroximer_core::timeline::Ticks;
use futures_util::future::BoxFuture;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Semaphore;

pub use failure::{Failure, Restriction, SESSION_ENDED_WARNING, classify};

use super::provider::{MAX_SEARCH_RESULTS, Provider, ProviderDescriptor};
use super::{
    FoundTrack, Listing, LoadOutcome, PlaylistInfo, ResolveError, ResolvedTrack, artwork,
    background, net,
};
use crate::config::MediaContext;
use crate::youtube::{Session, SessionCopy, SignInUnavailable};

mod activation;
mod entry;
mod failure;
mod profiles;

use activation::UrlActivation;
use entry::Entry;
use profiles::{SITES, SearchTarget, Site, is_youtube_target, site_of};

pub struct YtDlpProvider {
    site: &'static Site,
    descriptor: ProviderDescriptor,
}

impl YtDlpProvider {
    /// One provider per named site.
    pub fn sites() -> Vec<Self> {
        SITES.iter().copied().map(Self::new).collect()
    }

    /// Any other site yt-dlp supports; offered links last.
    pub fn generic() -> Self {
        Self::new(&profiles::GENERIC)
    }

    fn new(site: &'static Site) -> Self {
        Self {
            site,
            descriptor: ProviderDescriptor {
                name: site.name.to_owned(),
                display_name: site.display_name.to_owned(),
                search_prefixes: site.searches.iter().map(|s| s.prefix.to_owned()).collect(),
                hosts: site.hosts.iter().map(|&h| h.to_owned()).collect(),
            },
        }
    }

    fn is_generic(&self) -> bool {
        self.site.hosts.is_empty()
    }

    /// The site an entry belongs to: the named site of its link, else the loading one.
    fn site_for(&self, entry: &Entry) -> &'static Site {
        entry.uri().and_then(site_of).unwrap_or(self.site)
    }

    /// Fully extracts `uri` and prepares it for playback.
    async fn resolve_uri(
        &self,
        uri: &str,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let url = Url::parse(uri).map_err(|_| ResolveError::Unsupported(uri.to_owned()))?;
        self.guard(&url, ctx).await?;
        ytdlp_ready(ctx).await?;
        let (json, session) = self
            .extract_with_policy(uri, ctx, &["-J", "--no-playlist"])
            .await?;
        self.resolved(json, session, uri, ctx).await
    }

    /// Any link reaches the generic extractor, so it must not lead to internal services.
    async fn guard(&self, url: &Url, ctx: &MediaContext) -> Result<(), ResolveError> {
        if self.is_generic() && !ctx.config.sources.allow_private_urls {
            net::ensure_public(url).await?;
        }
        Ok(())
    }

    /// Extracts `target`, signed in to YouTube only as the auth policy allows. Returns the info
    /// JSON and the session it used, which the track's downloads keep using.
    async fn extract_with_policy(
        &self,
        target: &str,
        ctx: &MediaContext,
        args: &[&str],
    ) -> Result<(Value, Option<Session>), ResolveError> {
        // the session holds YouTube cookies only
        if !std::ptr::eq(self.site, &profiles::YOUTUBE) {
            return Ok((extract(ctx, target, None, args).await?, None));
        }
        let youtube = &ctx.youtube;
        let first = if youtube.sign_in_first(target) {
            youtube.checkout(ctx).map_err(ResolveError::Session)?
        } else {
            None
        };
        let restriction = match extract(ctx, target, first.as_ref(), args).await {
            Ok(json) => return Ok((json, first.as_ref().map(SessionCopy::session))),
            Err(ResolveError::Extraction {
                failure: Failure::Restricted(restriction),
                ..
            }) => restriction,
            Err(e) => return Err(e),
        };
        if first.is_some() {
            return Err(refused(ctx, target, restriction));
        }

        youtube.remember_restricted(target);
        let Some(session) = youtube.checkout(ctx).map_err(ResolveError::Session)? else {
            return Err(ResolveError::SignInRequired {
                query: target.to_owned(),
                restriction,
                reason: youtube.unavailable_reason(),
            });
        };
        tracing::debug!(target, ?restriction, "retrying signed in to YouTube");
        match extract(ctx, target, Some(&session), args).await {
            Ok(json) => Ok((json, Some(session.session()))),
            Err(ResolveError::Extraction {
                failure: Failure::Restricted(restriction),
                ..
            }) => Err(refused(ctx, target, restriction)),
            Err(e) => Err(e),
        }
    }

    /// A playlist's tracks with metadata only; they are resolved when played. Without artwork
    /// from yt-dlp (e.g. Bandcamp albums), the playlist's page provides it, and its tracks
    /// without any of their own share it.
    async fn playlist(
        &self,
        json: &Value,
        url: &Url,
        window: Window,
        ctx: &MediaContext,
    ) -> Result<LoadOutcome, ResolveError> {
        let Some((mut playlist, mut tracks)) = self.listing(json, url)? else {
            return match (window, listed_count(json)) {
                (Window::From { start, .. }, Some(total))
                    if start > 0 && start >= total as usize =>
                {
                    Err(ResolveError::PastEnd { total })
                }
                _ => Ok(LoadOutcome::Empty),
            };
        };
        if playlist.artwork_url.is_none()
            && let Some(page) = &playlist.uri
        {
            playlist.artwork_url = ctx.artwork.page_image(page, ctx).await;
        }
        artwork::inherit(&mut tracks, playlist.artwork_url.as_deref());
        window.fit(&mut playlist, &mut tracks);
        Ok(LoadOutcome::Playlist { playlist, tracks })
    }

    /// What yt-dlp lists of a playlist; `None` if it has no tracks.
    fn listing(
        &self,
        json: &Value,
        url: &Url,
    ) -> Result<Option<(PlaylistInfo, Vec<FoundTrack>)>, ResolveError> {
        let list = Entry::deserialize(json).map_err(|e| ResolveError::Json("yt-dlp", e))?;
        let tracks: Vec<FoundTrack> = entries(json)
            .filter(|entry| !entry.is_collection())
            .filter_map(|entry| entry.found_track(self.site_for(&entry)))
            .collect();
        if tracks.is_empty() {
            return Ok(None);
        }
        let playlist = PlaylistInfo {
            name: list.title.clone().unwrap_or_else(|| "Playlist".to_owned()),
            author: list.author(),
            uri: list.webpage_url.clone().or_else(|| Some(url.to_string())),
            artwork_url: list.artwork(),
            selected_track: selected_track(url, &tracks),
            total_tracks: list.playlist_count.and_then(|n| u32::try_from(n).ok()),
        };
        Ok(Some((playlist, tracks)))
    }

    /// A fully extracted track, ready to play. Without artwork from yt-dlp, its page's preview
    /// picture is used.
    async fn resolved(
        &self,
        mut json: Value,
        session: Option<Session>,
        requested: &str,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let mut entry = Entry::deserialize(&json).map_err(|e| ResolveError::Json("yt-dlp", e))?;
        if entry.uri().is_none() {
            entry.webpage_url = Some(requested.to_owned());
        }
        let mut track = entry
            .found_track(self.site_for(&entry))
            .ok_or_else(|| ResolveError::NoAudio(requested.to_owned()))?;
        if track.info.artwork_url.is_none() {
            track.info.artwork_url = ctx.artwork.page_image(&track.info.uri, ctx).await;
        }
        let media = Media::deserialize(&json).map_err(|e| ResolveError::Json("yt-dlp", e))?;
        let format = media
            .best_audio()
            .ok_or_else(|| ResolveError::NoAudio(requested.to_owned()))?;
        tracing::debug!(
            title = %track.info.title,
            format = ?format.format_id,
            protocol = ?format.protocol,
            "resolved with yt-dlp"
        );
        // see `activation`: the engine waits for the URL itself, so yt-dlp must not wait again
        let await_url = media.is_youtube() && format.transport() == Some(Transport::Stdout);
        if await_url {
            strip_available_at(&mut json);
        }
        // only a direct download is known to need nothing but its own format
        let keep_format = format
            .format_id
            .as_deref()
            .filter(|_| format.transport() == Some(Transport::Stdout));
        slim_info_json(&mut json, keep_format);
        let valid_until = format.url.as_deref().and_then(net::url_expiry);
        let producer = YtDlpProducer::new(
            ctx,
            Bytes::from(json.to_string()),
            &track.info.uri,
            &format,
            entry.is_live(),
            await_url,
            session,
        );
        Ok(ResolvedTrack {
            track,
            producer: Arc::new(producer),
            valid_until,
        })
    }
}

impl std::fmt::Debug for YtDlpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("YtDlpProvider")
            .field(&self.site.name)
            .finish()
    }
}

#[async_trait]
impl Provider for YtDlpProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn accepts(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && (self.is_generic() || url.host_str().is_some_and(|h| self.site.claims(h)))
    }

    async fn load_url(
        &self,
        url: &Url,
        listing: Listing,
        ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError> {
        self.guard(url, ctx).await?;
        ytdlp_ready(ctx).await?;
        let limit = ctx.config.sources.playlist_limit.max(1) as usize;
        let window = Window::new(listing, url, limit);
        let items = window.items(limit);
        let (json, session) = self
            .extract_with_policy(
                url.as_str(),
                ctx,
                &["-J", "--flat-playlist", "--playlist-items", &items],
            )
            .await?;
        let outcome = match json.get("_type").and_then(Value::as_str) {
            Some("playlist") => self.playlist(&json, url, window, ctx).await?,
            // a link standing for another one, e.g. a short link
            Some("url" | "url_transparent") => {
                let target = json
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ResolveError::NoAudio(url.to_string()))?;
                LoadOutcome::Track(self.resolve_uri(target, ctx).await?)
            }
            _ => LoadOutcome::Track(self.resolved(json, session, url.as_str(), ctx).await?),
        };
        Ok(Some(outcome))
    }

    async fn search(
        &self,
        prefix: &str,
        terms: &str,
        limit: usize,
        ctx: &MediaContext,
    ) -> Result<Vec<FoundTrack>, ResolveError> {
        let search = self
            .site
            .searches
            .iter()
            .find(|s| s.prefix == prefix)
            .ok_or_else(|| ResolveError::UnknownSearch(prefix.to_owned()))?;
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        ytdlp_ready(ctx).await?;
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS);
        let target = match search.target {
            SearchTarget::Key(key) => format!("{key}{limit}:{terms}"),
            SearchTarget::Page(page) => page(terms),
        };
        let items = format!("1:{limit}");
        // searches never carry the YouTube session, keeping the account out of that traffic
        let output = run(
            ctx,
            Purpose::Search,
            &target,
            None,
            &["-J", "--flat-playlist", "--playlist-items", &items],
        )
        .await?;
        let json: Value =
            serde_json::from_slice(&output.stdout).map_err(|e| ResolveError::Json("yt-dlp", e))?;
        Ok(entries(&json)
            .filter(|entry| !entry.is_collection())
            .filter_map(|entry| entry.found_track(self.site_for(&entry)))
            .take(limit)
            .collect())
    }

    async fn resolve(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        self.resolve_uri(&track.reference.uri, ctx).await
    }
}

/// Which entries of a playlist yt-dlp is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Window {
    /// Up to the limit from the first: for [`Listing::Whole`], and for a queue's listing of a
    /// link that names its track by ID only (`v=`), whose index only the listing tells.
    Whole { from_selected: bool },
    /// `count` entries from index `start`, for a queue.
    From { start: usize, count: usize },
}

impl Window {
    /// The start comes from the queue, else the link's 1-based `index=`, else its `v=`. The
    /// engine's `limit` counts from the start.
    fn new(listing: Listing, url: &Url, limit: usize) -> Self {
        let Listing::Queue {
            start_index,
            limit: wanted,
        } = listing
        else {
            return Self::Whole {
                from_selected: false,
            };
        };
        let query: HashMap<_, _> = url.query_pairs().collect();
        let index = query
            .get("index")
            .and_then(|index| index.parse::<usize>().ok())
            .and_then(|index| index.checked_sub(1));
        let count = wanted.unwrap_or(limit).clamp(1, limit);
        match start_index.or(index) {
            Some(start) => Self::From { start, count },
            None if query.contains_key("v") => Self::Whole {
                from_selected: true,
            },
            None => Self::From { start: 0, count },
        }
    }

    /// yt-dlp's `--playlist-items`, which counts from 1.
    fn items(self, limit: usize) -> String {
        match self {
            Self::Whole { .. } => format!("1:{limit}"),
            Self::From { start, count } => format!("{}:{}", start + 1, start + count),
        }
    }

    /// Cuts a queue's listing of the whole playlist to start at the linked track.
    fn fit(self, playlist: &mut PlaylistInfo, tracks: &mut Vec<FoundTrack>) {
        match self {
            Self::Whole {
                from_selected: false,
            } => {}
            Self::Whole {
                from_selected: true,
            } => {
                let selected = playlist.selected_track.take().unwrap_or(0) as usize;
                tracks.drain(..selected.min(tracks.len()));
            }
            // the tracks start at the start track
            Self::From { .. } => playlist.selected_track = None,
        }
    }
}

/// How many tracks yt-dlp says a playlist has.
fn listed_count(json: &Value) -> Option<u32> {
    Entry::deserialize(json)
        .ok()?
        .playlist_count
        .and_then(|count| u32::try_from(count).ok())
}

/// Waits for the managed yt-dlp's first install, if it is still running.
async fn ytdlp_ready(ctx: &MediaContext) -> Result<(), ResolveError> {
    ctx.ytdlp
        .ready()
        .await
        .map_err(ResolveError::YtDlpUnavailable)
}

/// The entries of a playlist or search result.
fn entries(json: &Value) -> impl Iterator<Item = Entry> + '_ {
    json.get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| Entry::deserialize(entry).ok())
}

/// The track a playlist link points at: YouTube's `v=`, or a 1-based `index=`.
fn selected_track(url: &Url, tracks: &[FoundTrack]) -> Option<u32> {
    let query: HashMap<_, _> = url.query_pairs().collect();
    let position = match (query.get("v"), query.get("index")) {
        (Some(id), _) => tracks.iter().position(|t| t.info.identifier == **id),
        (None, Some(index)) => index
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1))
            .filter(|&i| i < tracks.len()),
        (None, None) => None,
    };
    position.and_then(|p| u32::try_from(p).ok())
}

/// YouTube still wants an account although the session was sent.
fn refused(ctx: &MediaContext, query: &str, restriction: Restriction) -> ResolveError {
    if restriction.implicates_session() {
        ctx.youtube.request_check();
    }
    ResolveError::SignInRequired {
        query: query.to_owned(),
        restriction,
        reason: SignInUnavailable::Refused,
    }
}

async fn extract(
    ctx: &MediaContext,
    target: &str,
    session: Option<&SessionCopy>,
    args: &[&str],
) -> Result<Value, ResolveError> {
    let output = run(ctx, Purpose::Extraction, target, session, args).await?;
    serde_json::from_slice(&output.stdout).map_err(|e| ResolveError::Json("yt-dlp", e))
}

/// Loads `video` signed in, without downloading; returns yt-dlp's warnings.
pub async fn probe(
    ctx: &MediaContext,
    video: &str,
    session: &SessionCopy,
) -> Result<String, ResolveError> {
    let output = run(
        ctx,
        Purpose::Extraction,
        video,
        Some(session),
        &["--simulate", "--no-playlist", "--print", "id"],
    )
    .await?;
    Ok(output.stderr)
}

struct Output {
    stdout: Vec<u8>,
    stderr: String,
}

/// What a yt-dlp run is for, which decides whose turns it waits for and how long it may take.
#[derive(Clone, Copy, Debug)]
enum Purpose {
    Search,
    /// Loading a link, resolving a track to play, or checking the YouTube session.
    Extraction,
}

impl Purpose {
    fn turns(self, ctx: &MediaContext) -> &Semaphore {
        match self {
            Self::Search => &ctx.ytdlp_runs.searches,
            Self::Extraction => &ctx.ytdlp_runs.extractions,
        }
    }

    /// Long enough for a playlist of hundreds of entries, or YouTube's JavaScript challenges on a
    /// slow host; a run that hangs (a stalled connection) would otherwise hold its turn forever.
    fn timeout(self) -> Duration {
        match self {
            Self::Search => Duration::from_secs(30),
            Self::Extraction => Duration::from_secs(120),
        }
    }
}

/// Runs yt-dlp on `target` once its purpose's turn comes (a background turn for a background
/// load). Warnings stay on so that yt-dlp can report a session YouTube ended.
async fn run(
    ctx: &MediaContext,
    purpose: Purpose,
    target: &str,
    session: Option<&SessionCopy>,
    args: &[&str],
) -> Result<Output, ResolveError> {
    let _turn = background::turn(purpose.turns(ctx), &ctx.ytdlp_runs.background).await;
    let mut command = ctx.ytdlp.command();
    command.args(&ctx.config.ytdlp.args.0).args(args);
    if let Some(proxy) = ctx.youtube.proxy().filter(|_| is_youtube_target(target)) {
        command.arg("--proxy").arg(proxy.as_str());
    }
    if let Some(session) = session {
        command.arg("--cookies").arg(session.path());
    }
    // dropped on timeout, which kills the process
    let running = command.arg("--").arg(target).kill_on_drop(true).output();
    let output = tokio::time::timeout(purpose.timeout(), running)
        .await
        .map_err(|_| ResolveError::Timeout {
            program: "yt-dlp",
            after: purpose.timeout(),
        })?
        .map_err(|e| ResolveError::Spawn("yt-dlp", e))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if let Some(session) = session {
        ctx.youtube.observe(session, &stderr);
    }
    if !output.status.success() {
        let message = error_line(&stderr);
        return Err(ResolveError::Extraction {
            query: target.to_owned(),
            failure: classify(&message),
            message,
        });
    }
    Ok(Output {
        stdout: output.stdout,
        stderr,
    })
}

/// The formats of a fully extracted track.
#[derive(Debug, Deserialize)]
struct Media {
    extractor_key: Option<String>,
    #[serde(default)]
    formats: Vec<Format>,
    // a single-format result (a direct media link) carries its format at the top level
    #[serde(flatten)]
    direct: Format,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Format {
    format_id: Option<String>,
    ext: Option<String>,
    acodec: Option<String>,
    vcodec: Option<String>,
    abr: Option<f64>,
    protocol: Option<String>,
    url: Option<String>,
    #[serde(default)]
    http_headers: HashMap<String, String>,
    /// When YouTube makes the URL available, in Unix seconds; see `activation`.
    available_at: Option<f64>,
}

impl Format {
    fn is_audio_only(&self) -> bool {
        self.vcodec.as_deref() == Some("none") && self.acodec.as_deref() != Some("none")
    }

    fn transport(&self) -> Option<Transport> {
        match self.protocol.as_deref()? {
            "https" | "http" => Some(Transport::Stdout),
            "sabr" => Some(Transport::GrowingFile),
            // ffmpeg reads the playlist from its URL
            "m3u8" | "m3u8_native" if self.url.is_some() => Some(Transport::Hls),
            _ => None,
        }
    }

    /// Opus first (Discord's codec, no transcoding loss), then full dynamic range, then
    /// seekable `https` over `sabr`, then bitrate.
    fn preference(&self) -> (bool, bool, bool, f64) {
        let id = self.format_id.as_deref().unwrap_or_default();
        (
            self.acodec.as_deref() == Some("opus"),
            !id.ends_with("-drc"),
            self.transport() == Some(Transport::Stdout),
            self.abr.unwrap_or(0.0),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Stdout,
    GrowingFile,
    Hls,
}

impl Media {
    fn is_youtube(&self) -> bool {
        self.extractor_key.as_deref() == Some("Youtube")
    }

    fn best_audio(&self) -> Option<Format> {
        if self.formats.is_empty() {
            return self
                .direct
                .transport()
                .is_some()
                .then(|| self.direct.clone());
        }
        self.formats
            .iter()
            .filter(|f| f.is_audio_only() && f.transport().is_some())
            .max_by(|a, b| {
                let (a, b) = (a.preference(), b.preference());
                (a.0, a.1, a.2)
                    .cmp(&(b.0, b.1, b.2))
                    .then(a.3.total_cmp(&b.3))
            })
            .cloned()
    }
}

/// Downloads with yt-dlp (or ffmpeg, for HLS), and re-opens at an offset through ffmpeg.
struct YtDlpProducer {
    download: Arc<dyn Producer>,
    seek: Option<FfmpegProducer>,
    live: bool,
    /// Downloads wait for YouTube to accept the format's freshly issued URL.
    activation: Option<Arc<UrlActivation>>,
}

impl YtDlpProducer {
    fn new(
        ctx: &MediaContext,
        info_json: Bytes,
        page: &str,
        format: &Format,
        live: bool,
        await_url: bool,
        session: Option<Session>,
    ) -> Self {
        let media = &ctx.config;
        let transport = format
            .transport()
            .expect("best_audio only picks supported protocols");
        // YouTube's media URLs only work from the address that extracted them
        let proxy = ctx
            .youtube
            .proxy()
            .filter(|_| is_youtube_target(page))
            .map(|proxy| proxy.to_string());
        let direct = format.url.as_ref().map(|url| FfmpegProducer {
            headers: format
                .http_headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            live,
            http_proxy: proxy.clone(),
            ..FfmpegProducer::new(&media.ffmpeg, url)
        });
        let caps = ProducerCaps {
            time_seek: false,
            live,
        };
        let hint = format.ext.clone();
        let base_args = || -> Vec<String> {
            let mut args = media.ytdlp.args.0.clone();
            args.extend(["--quiet", "--no-warnings", "--no-progress"].map(String::from));
            if let Some(proxy) = &proxy {
                args.extend(["--proxy".to_owned(), proxy.clone()]);
            }
            args
        };
        let work_dir = ctx.work_dir().to_owned();

        let download: Arc<dyn Producer> = match transport {
            Transport::Stdout => {
                let mut spec = ctx
                    .ytdlp
                    .spec()
                    .args(base_args())
                    .args(["--load-info-json", "-", "-o", "-"])
                    .stdin(info_json);
                if let Some(id) = &format.format_id {
                    spec = spec.args(["-f", id]);
                }
                Arc::new(ProcessProducer::new(caps, hint, move |_| {
                    let spec = signed_in(spec.clone(), session.as_ref(), &work_dir);
                    (spec, TimestampMode::Absolute)
                }))
            }
            Transport::GrowingFile => {
                let (ytdlp, mut args) = (ctx.ytdlp.spec(), base_args());
                // extracted again, the format may be missing or come over `https` instead; the
                // best audio left is as good
                let formats = match &format.format_id {
                    Some(id) => format!("{id}/bestaudio"),
                    None => "bestaudio".to_owned(),
                };
                args.extend(["--no-playlist".to_owned(), "-f".to_owned(), formats]);
                let page = page.to_owned();
                Arc::new(GrowingFileProducer::new(
                    ctx.work_dir().to_owned(),
                    caps,
                    hint,
                    move |_, dir: &Path| {
                        let template = dir.join("media.%(ext)s");
                        let spec = ytdlp
                            .clone()
                            .args(args.clone())
                            .arg("-o")
                            .arg(template.to_string_lossy());
                        // options must come before `--`
                        let spec = signed_in(spec, session.as_ref(), &work_dir).args(["--", &page]);
                        (spec, TimestampMode::Absolute)
                    },
                ))
            }
            Transport::Hls => Arc::new(
                direct
                    .clone()
                    .expect("HLS formats always carry their playlist URL"),
            ),
        };
        let expected = format
            .available_at
            .and_then(|at| Duration::try_from_secs_f64(at).ok())
            .map(|since_epoch| UNIX_EPOCH + since_epoch);
        let activation = direct.as_ref().filter(|_| await_url).map(|direct| {
            let client = if proxy.is_some() {
                ctx.youtube.client().clone()
            } else {
                net::client().clone()
            };
            Arc::new(UrlActivation::new(
                client,
                direct.input.clone(),
                direct.headers.clone(),
                expected,
            ))
        });
        // live streams cannot seek; SABR has no URL to seek with
        let seek = direct.filter(|_| transport != Transport::GrowingFile && !live);
        Self {
            download,
            seek,
            live,
            activation,
        }
    }
}

/// Signs one download into the session its track was resolved with: a fresh copy of the
/// stored cookies, merged back once the process has exited.
fn signed_in(spec: ProcessSpec, session: Option<&Session>, work_dir: &Path) -> ProcessSpec {
    let Some(session) = session else {
        return spec;
    };
    match session.checkout(work_dir) {
        Ok(Some(copy)) => spec
            .arg("--cookies")
            .arg(copy.path().to_string_lossy())
            .guard(copy),
        // replaced or removed since the track was resolved
        Ok(None) => spec,
        Err(e) => {
            tracing::warn!(
                error = %display_chain(&e),
                "cannot copy the YouTube session for a download"
            );
            spec
        }
    }
}

impl Producer for YtDlpProducer {
    fn caps(&self) -> ProducerCaps {
        ProducerCaps {
            time_seek: self.seek.is_some(),
            live: self.live,
        }
    }

    fn open(&self, start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        // both only start their process once the returned future runs
        let opening = match (start.filter(|&t| t > 0), &self.seek) {
            (Some(start), Some(seek)) => seek.open(Some(start)),
            _ => self.download.open(None),
        };
        match &self.activation {
            Some(activation) => {
                let activation = Arc::clone(activation);
                Box::pin(async move {
                    activation.wait().await;
                    opening.await
                })
            }
            None => opening,
        }
    }
}

/// Top-level keys of yt-dlp's info JSON that downloading never reads. They are most of it:
/// YouTube's automatic captions alone take 500 KB of a typical 600 KB.
const UNUSED_INFO_KEYS: &[&str] = &[
    "automatic_captions",
    "subtitles",
    "requested_subtitles",
    "heatmap",
    "thumbnails",
    "description",
    "chapters",
    "tags",
    "categories",
    "requested_downloads",
    "requested_formats",
];

/// Cuts yt-dlp's info JSON down to what the track's downloads hand back to yt-dlp, since it is
/// kept as long as the track (in the cache of resolved tracks, too). With `keep_format`, only
/// that format remains.
fn slim_info_json(json: &mut Value, keep_format: Option<&str>) {
    let Some(top) = json.as_object_mut() else {
        return;
    };
    for key in UNUSED_INFO_KEYS {
        top.remove(*key);
    }
    if let Some(id) = keep_format
        && let Some(formats) = top.get_mut("formats").and_then(Value::as_array_mut)
    {
        formats.retain(|format| format.get("format_id").and_then(Value::as_str) == Some(id));
    }
}

/// Removes `available_at` from every format of yt-dlp's info JSON; see `activation`.
fn strip_available_at(json: &mut Value) {
    if let Some(top) = json.as_object_mut() {
        top.remove("available_at");
    }
    for key in ["formats", "requested_formats"] {
        for format in json
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(format) = format.as_object_mut() {
                format.remove("available_at");
            }
        }
    }
}

/// The most relevant line of yt-dlp's error output: its last `ERROR:` line, or else the last line.
fn error_line(stderr: &str) -> String {
    let mut lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    lines
        .clone()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| lines.next_back())
        .unwrap_or("no error output")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sources::TrackRef;

    fn media(formats: Value) -> Media {
        Media::deserialize(json!({ "id": "x", "formats": formats })).unwrap()
    }

    fn audio(id: &str, acodec: &str, abr: f64, protocol: &str) -> Value {
        json!({ "format_id": id, "acodec": acodec, "vcodec": "none", "abr": abr, "protocol": protocol, "url": "https://example.invalid" })
    }

    #[test]
    fn slims_the_info_json_to_what_downloads_need() {
        let full = json!({
            "id": "x",
            "extractor": "youtube",
            "automatic_captions": { "en": [{ "url": "https://example.invalid/captions" }] },
            "thumbnails": [{ "url": "https://example.invalid/t.jpg" }],
            "requested_formats": [{ "format_id": "251" }],
            "formats": [{ "format_id": "140" }, { "format_id": "251", "protocol": "https" }],
        });

        let mut direct = full.clone();
        slim_info_json(&mut direct, Some("251"));
        assert_eq!(
            direct,
            json!({
                "id": "x",
                "extractor": "youtube",
                "formats": [{ "format_id": "251", "protocol": "https" }],
            })
        );

        let mut sabr = full;
        slim_info_json(&mut sabr, None);
        assert_eq!(sabr["formats"].as_array().unwrap().len(), 2);
        assert!(sabr.get("automatic_captions").is_none());
    }

    #[test]
    fn strips_available_at_everywhere() {
        let mut json = json!({
            "available_at": 1,
            "formats": [{ "format_id": "251", "available_at": 2 }, { "format_id": "140" }],
            "requested_formats": [{ "format_id": "251", "available_at": 3 }],
        });
        strip_available_at(&mut json);
        assert!(!json.to_string().contains("available_at"), "{json}");
        assert_eq!(json["formats"][0]["format_id"], "251");
    }

    #[test]
    fn prefers_opus_full_range_seekable_then_bitrate() {
        let i = media(json!([
            audio("140", "mp4a.40.2", 129.0, "https"),
            audio("251-drc", "opus", 107.0, "https"),
            audio("251", "opus", 106.0, "https"),
            audio("250", "opus", 60.0, "https"),
            { "format_id": "18", "acodec": "mp4a", "vcodec": "avc1", "protocol": "https" },
        ]));
        assert_eq!(i.best_audio().unwrap().format_id.as_deref(), Some("251"));

        let https_beats_higher_bitrate_sabr = media(json!([
            audio("251-sabr", "opus", 160.0, "sabr"),
            audio("251", "opus", 106.0, "https"),
        ]));
        assert_eq!(
            https_beats_higher_bitrate_sabr
                .best_audio()
                .unwrap()
                .format_id
                .as_deref(),
            Some("251")
        );

        let sabr_only = media(json!([audio("251", "opus", 160.0, "sabr")]));
        assert_eq!(
            sabr_only.best_audio().unwrap().transport(),
            Some(Transport::GrowingFile)
        );

        let soundcloud = media(json!([
            audio("hls_aac_96k", "mp4a.40.2", 96.0, "m3u8_native"),
            audio("hls_aac_160k", "mp4a.40.2", 160.0, "m3u8_native"),
        ]));
        assert_eq!(
            soundcloud.best_audio().unwrap().format_id.as_deref(),
            Some("hls_aac_160k")
        );

        assert!(
            media(json!([audio("x", "opus", 1.0, "f4m")]))
                .best_audio()
                .is_none()
        );
    }

    #[test]
    fn playlists_list_their_tracks_with_metadata() {
        let json: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ytdlp/youtube_playlist.json"
        ))
        .unwrap();
        let url =
            Url::parse("https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI")
                .unwrap();
        let (playlist, tracks) = YtDlpProvider::new(&profiles::YOUTUBE)
            .listing(&json, &url)
            .unwrap()
            .expect("the fixture has tracks");
        assert_eq!(playlist.name, "Popular Music Videos");
        assert_eq!(playlist.author.as_deref(), Some("Music"));
        assert_eq!(playlist.total_tracks, Some(182));
        assert_eq!(tracks.len(), 3, "the fixture holds the first three");
        assert!(
            tracks
                .iter()
                .all(|t| t.reference.source == "youtube" && t.info.length.is_some())
        );
    }

    #[test]
    fn queues_list_from_their_start_track() {
        let url = |u: &str| Url::parse(u).unwrap();
        let queue = |start_index| Listing::Queue {
            start_index,
            limit: None,
        };
        let list = url("https://www.youtube.com/playlist?list=PL1");
        assert_eq!(Window::new(Listing::Whole, &list, 500).items(500), "1:500");
        assert_eq!(Window::new(queue(None), &list, 500).items(500), "1:500");
        assert_eq!(
            Window::new(queue(Some(199)), &list, 500).items(500),
            "200:699",
            "the limit counts from the start"
        );
        let one = Listing::Queue {
            start_index: Some(199),
            limit: Some(1),
        };
        assert_eq!(Window::new(one, &list, 500).items(500), "200:200");

        // a link's own index, unless the queue gives a start
        let indexed = url("https://www.youtube.com/watch?v=b&list=PL1&index=7");
        assert_eq!(Window::new(queue(None), &indexed, 500).items(500), "7:506");
        assert_eq!(
            Window::new(queue(Some(0)), &indexed, 500).items(500),
            "1:500"
        );
        // a video ID alone tells no index: everything is listed, from that video on
        let by_id = url("https://www.youtube.com/watch?v=b&list=PL1");
        assert_eq!(
            Window::new(queue(None), &by_id, 500),
            Window::Whole {
                from_selected: true
            }
        );
    }

    #[test]
    fn a_queue_listing_starts_at_the_linked_video() {
        let json: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ytdlp/youtube_playlist.json"
        ))
        .unwrap();
        let url =
            Url::parse("https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI")
                .unwrap();
        let provider = YtDlpProvider::new(&profiles::YOUTUBE);
        let second = provider.listing(&json, &url).unwrap().unwrap().1[1]
            .info
            .identifier
            .clone();
        let by_id = Url::parse(&format!("{url}&v={second}")).unwrap();
        let (mut playlist, mut tracks) = provider.listing(&json, &by_id).unwrap().unwrap();
        Window::Whole {
            from_selected: true,
        }
        .fit(&mut playlist, &mut tracks);
        assert_eq!(
            tracks[0].info.identifier, second,
            "from the linked video on"
        );
        assert_eq!(tracks.len(), 2);
        assert_eq!(playlist.selected_track, None);
    }

    /// Lists a real YouTube playlist for a queue, from a start track. Needs the network and
    /// yt-dlp, so it only runs when asked: `cargo test -- --ignored from_a_start`.
    #[tokio::test]
    #[ignore = "lists a YouTube playlist"]
    async fn lists_a_youtube_playlist_from_a_start() {
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
        let provider = YtDlpProvider::new(&profiles::YOUTUBE);
        let url =
            Url::parse("https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI")
                .unwrap();
        let listing = Listing::Queue {
            start_index: Some(100),
            limit: None,
        };
        let Some(LoadOutcome::Playlist { playlist, tracks }) =
            provider.load_url(&url, listing, &ctx).await.unwrap()
        else {
            panic!("a playlist");
        };
        let total = playlist.total_tracks.expect("YouTube tells how many") as usize;
        assert!(total > 150, "{total} tracks");
        assert_eq!(tracks.len(), total - 100, "from the 101st on");
    }

    #[test]
    fn playlist_links_select_their_track() {
        let track = |id: &str| FoundTrack {
            reference: TrackRef {
                source: "youtube".into(),
                uri: format!("https://www.youtube.com/watch?v={id}"),
            },
            info: Entry {
                id: Some(id.into()),
                webpage_url: Some(format!("https://www.youtube.com/watch?v={id}")),
                ..Entry::default()
            }
            .found_track(&profiles::YOUTUBE)
            .unwrap()
            .info,
        };
        let tracks = [track("a"), track("b"), track("c")];
        let url = |u: &str| Url::parse(u).unwrap();
        assert_eq!(
            selected_track(&url("https://www.youtube.com/watch?v=b&list=PL1"), &tracks),
            Some(1)
        );
        assert_eq!(
            selected_track(
                &url("https://www.youtube.com/playlist?list=PL1&index=3"),
                &tracks
            ),
            Some(2)
        );
        assert_eq!(
            selected_track(&url("https://www.youtube.com/playlist?list=PL1"), &tracks),
            None
        );
    }
}
