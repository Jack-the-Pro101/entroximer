//! Providers and the registry that routes user input to them.
//!
//! - A URL goes to the first provider that accepts it and does not decline it: the site
//!   providers by host (and Spotify, which also takes `spotify:` URIs), `upload:` URIs if uploads
//!   are on, then `http` for links to media files, then yt-dlp's generic extractor.
//! - `prefix:terms` with a known search prefix (`ytsearch:`, `ytmsearch:`, `scsearch:`,
//!   `spsearch:`) searches with the provider that owns the prefix.
//! - A path on the engine's host plays the file, if local files are allowed.
//! - Anything else is searched with the default search prefix.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Url;
use serde::Serialize;
use ts_rs::TS;

use super::direct::{HttpProvider, LocalProvider};
use super::spotify::SpotifyProvider;
use super::upload::UploadProvider;
use super::ytdlp::YtDlpProvider;
use super::{FoundTrack, LoadOutcome, ResolveError, ResolvedTrack};
use crate::config::{MediaContext, SourcesConfig, SpotifyConfig};
use crate::uploads::UploadStore;

/// Results of searches made through `tracks.load`.
pub const DEFAULT_SEARCH_LIMIT: usize = 10;
/// Most results one search may ask for.
pub const MAX_SEARCH_RESULTS: usize = 50;

/// A source of tracks, as clients see it in `node.info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProviderDescriptor {
    /// The name tracks carry as their `sourceName`, e.g. `youtube`.
    pub name: String,
    pub display_name: String,
    /// What to put before search terms, e.g. `ytsearch`, as in `ytsearch:never gonna`.
    pub search_prefixes: Vec<String>,
    /// Sites whose links the provider plays; empty for providers that take any link.
    pub hosts: Vec<String>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;

    /// Whether `url` is worth offering to this provider.
    fn accepts(&self, url: &Url) -> bool;

    /// Loads a track or playlist. `None` passes the URL on to the next provider, e.g. a page
    /// that is not a media file.
    async fn load_url(
        &self,
        url: &Url,
        ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError>;

    /// Searches with one of the provider's search prefixes.
    async fn search(
        &self,
        prefix: &str,
        _terms: &str,
        _limit: usize,
        _ctx: &MediaContext,
    ) -> Result<Vec<FoundTrack>, ResolveError> {
        Err(ResolveError::UnknownSearch(prefix.to_owned()))
    }

    /// Makes a track it produced playable again, with its full metadata. `track` is what the
    /// provider reported for it before, e.g. as a search result.
    async fn resolve(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError>;
}

/// Search results and the search that produced them.
#[derive(Debug)]
pub struct SearchOutcome {
    pub prefix: String,
    pub provider: String,
    pub tracks: Vec<FoundTrack>,
}

/// Every provider, in the order URLs are offered to them.
pub struct Sources {
    providers: Vec<Arc<dyn Provider>>,
    /// Also among `providers`, for its startup check.
    spotify: Option<Arc<SpotifyProvider>>,
    default_search: String,
    local_files: bool,
}

impl Sources {
    pub fn new(
        config: &SourcesConfig,
        spotify: &SpotifyConfig,
        uploads: Option<Arc<UploadStore>>,
    ) -> Self {
        let mut providers: Vec<Arc<dyn Provider>> = YtDlpProvider::sites()
            .into_iter()
            .map(|site| Arc::new(site) as Arc<dyn Provider>)
            .collect();
        let spotify = SpotifyProvider::new(spotify).map(Arc::new);
        if let Some(spotify) = &spotify {
            providers.push(Arc::clone(spotify) as Arc<dyn Provider>);
        }
        if let Some(uploads) = uploads {
            providers.push(Arc::new(UploadProvider::new(uploads)));
        }
        providers.push(Arc::new(HttpProvider::new()));
        providers.push(Arc::new(YtDlpProvider::generic()));
        if config.allow_local_files {
            providers.push(Arc::new(LocalProvider::new()));
        }
        Self {
            providers,
            spotify,
            default_search: config.default_search.clone(),
            local_files: config.allow_local_files,
        }
    }

    /// Logs the providers, and whether Spotify accepts the configured app, so that operators
    /// learn of a mistake at startup rather than from the first user who tries.
    pub async fn report(&self) {
        let names: Vec<&str> = self
            .providers
            .iter()
            .map(|p| p.descriptor().name.as_str())
            .collect();
        tracing::info!(providers = %names.join(", "), "sources");
        match &self.spotify {
            None => tracing::info!(
                "Spotify is off; set ENTROXIMER_SPOTIFY_CLIENT_ID and ENTROXIMER_SPOTIFY_CLIENT_SECRET to enable it"
            ),
            Some(spotify) => match spotify.check().await {
                Ok(()) => tracing::info!("Spotify accepts the app's credentials"),
                Err(e) => tracing::warn!(
                    error = %entroximer_core::error::display_chain(&e),
                    "Spotify links and searches will fail"
                ),
            },
        }
    }

    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .iter()
            .map(|p| p.descriptor().clone())
            .collect()
    }

    /// Loads a URL, a path, or a search (`prefix:terms`, or plain text with the default search).
    pub async fn load(&self, query: &str, ctx: &MediaContext) -> Result<LoadOutcome, ResolveError> {
        let query = query.trim();
        if let Some(url) = self.as_url(query) {
            for provider in self.providers.iter().filter(|p| p.accepts(&url)) {
                if let Some(outcome) = provider.load_url(&url, ctx).await? {
                    return Ok(outcome);
                }
            }
            return Err(ResolveError::Unsupported(query.to_owned()));
        }
        let (prefix, terms) = self.split_search(query);
        let tracks = self
            .search(Some(prefix), terms, DEFAULT_SEARCH_LIMIT, ctx)
            .await?
            .tracks;
        Ok(if tracks.is_empty() {
            LoadOutcome::Empty
        } else {
            LoadOutcome::Search(tracks)
        })
    }

    /// Searches with `prefix` (the default search if `None`) for at most `limit` tracks.
    pub async fn search(
        &self,
        prefix: Option<&str>,
        terms: &str,
        limit: usize,
        ctx: &MediaContext,
    ) -> Result<SearchOutcome, ResolveError> {
        let prefix = prefix.unwrap_or(&self.default_search);
        let provider = self
            .owner_of(prefix)
            .ok_or_else(|| ResolveError::UnknownSearch(prefix.to_owned()))?;
        let tracks = provider.search(prefix, terms.trim(), limit, ctx).await?;
        Ok(SearchOutcome {
            prefix: prefix.to_owned(),
            provider: provider.descriptor().name.clone(),
            tracks,
        })
    }

    /// Makes a track loaded earlier playable.
    pub async fn resolve(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let source = &track.reference.source;
        let provider = self
            .providers
            .iter()
            .find(|p| &p.descriptor().name == source)
            .ok_or_else(|| ResolveError::UnknownSource(source.clone()))?;
        provider.resolve(track, ctx).await
    }

    /// Loads `query` and makes one track of it playable: a search's first result, or the track a
    /// playlist link pointed at (else its first).
    pub async fn resolve_query(
        &self,
        query: &str,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let found = match self.load(query, ctx).await? {
            LoadOutcome::Track(resolved) => return Ok(resolved),
            LoadOutcome::Playlist { playlist, tracks } => {
                let selected = playlist.selected_track.unwrap_or(0) as usize;
                tracks.into_iter().nth(selected)
            }
            LoadOutcome::Search(tracks) => tracks.into_iter().next(),
            LoadOutcome::Empty => None,
        };
        let found = found.ok_or_else(|| ResolveError::NoResults(query.to_owned()))?;
        self.resolve(&found, ctx).await
    }

    /// Web links, URIs a provider takes (e.g. `spotify:track:...`), and paths on the engine's
    /// host when local files are allowed.
    fn as_url(&self, query: &str) -> Option<Url> {
        if let Ok(url) = Url::parse(query) {
            let web = matches!(url.scheme(), "http" | "https");
            // files only by their path, which is checked below
            let taken = url.scheme() != "file" && self.providers.iter().any(|p| p.accepts(&url));
            if web || taken {
                return Some(url);
            }
        }
        if !self.local_files {
            return None;
        }
        let path = std::fs::canonicalize(Path::new(query)).ok()?;
        path.is_file()
            .then(|| Url::from_file_path(path).ok())
            .flatten()
    }

    /// `ytsearch:terms` with a known prefix, else the whole query with the default search.
    fn split_search<'q>(&'q self, query: &'q str) -> (&'q str, &'q str) {
        match query.split_once(':') {
            Some((prefix, terms)) if self.owner_of(prefix).is_some() => (prefix, terms),
            _ => (&self.default_search, query),
        }
    }

    fn owner_of(&self, prefix: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.iter().find(|p| {
            p.descriptor()
                .search_prefixes
                .iter()
                .any(|known| known == prefix)
        })
    }
}

impl std::fmt::Debug for Sources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sources")
            .field(
                "providers",
                &self
                    .providers
                    .iter()
                    .map(|p| p.descriptor().name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("default_search", &self.default_search)
            .finish()
    }
}
