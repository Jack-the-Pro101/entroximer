//! Artwork for every track, wherever it comes from. In order of preference:
//!
//! 1. What the provider reports: YouTube thumbnails (by video ID, see `ytdlp::profiles`),
//!    SoundCloud and Spotify artwork, thumbnails yt-dlp found.
//! 2. For a playlist's tracks without any, the playlist's, e.g. a Bandcamp album's cover.
//! 3. For pages without either, the page's own preview picture (`og:image`), as sites show in
//!    link previews: [`Artwork::page_image`].
//! 4. For media files (direct links, local files), the cover embedded in the file, which the
//!    engine extracts and serves itself at `/v1/artwork/<key>`: [`Artwork::cover`]. Such an
//!    `artworkUrl` is a path on the engine, fetched with the engine's `Authorization`.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use reqwest::Url;
use reqwest::header::CONTENT_TYPE;
use tokio::sync::Semaphore;

use super::{FoundTrack, ResolveError, TrackRef, direct, net};
use crate::config::MediaContext;
use crate::expiring::Expiring;

/// Where the engine serves embedded covers; the key follows.
pub const COVER_PATH: &str = "/v1/artwork/";

const PAGE_TIMEOUT: Duration = Duration::from_secs(5);
/// Preview pictures are declared in a page's head; reading stops there or after this much.
const PAGE_HEAD_BYTES: usize = 256 << 10;
const KEEP_FOR: Duration = Duration::from_secs(3600);
const REMEMBERED_PAGES: usize = 1024;
/// Covers are small JPEGs, so these take a few megabytes at most.
const REMEMBERED_COVERS: usize = 64;
/// Cover extractions at once: each is an ffprobe and an ffmpeg run.
const COVER_RUNS: usize = 4;

/// An embedded cover, as served.
#[derive(Clone, Debug)]
pub struct Cover {
    pub data: Bytes,
    pub content_type: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum CoverError {
    #[error("not an artwork key")]
    BadKey,
    #[error("there is no artwork here")]
    NotFound,
    #[error(transparent)]
    Source(#[from] ResolveError),
}

/// Artwork the engine finds itself, with caches so that it looks for each only once a while.
#[derive(Debug)]
pub struct Artwork {
    /// Preview pictures by page URL; `None` for pages without one.
    page_images: Expiring<String, Option<String>>,
    /// Embedded covers by key; `None` for files without one.
    covers: Expiring<String, Option<Cover>>,
    cover_runs: Semaphore,
}

impl Default for Artwork {
    fn default() -> Self {
        Self {
            page_images: Expiring::new(KEEP_FOR, REMEMBERED_PAGES),
            covers: Expiring::new(KEEP_FOR, REMEMBERED_COVERS),
            cover_runs: Semaphore::new(COVER_RUNS),
        }
    }
}

impl Artwork {
    /// The preview picture of the web page `page`, if it declares one. Failures count as none:
    /// artwork is never worth failing a load for.
    pub async fn page_image(&self, page: &str, ctx: &MediaContext) -> Option<String> {
        if let Some(known) = self.page_images.get(page) {
            return known;
        }
        let found = fetch_page_image(page, ctx).await;
        self.page_images.insert(page.to_owned(), found.clone());
        found
    }

    /// The cover embedded in the media file behind `key` (see [`cover_path`]).
    pub async fn cover(&self, key: &str, ctx: &MediaContext) -> Result<Cover, CoverError> {
        if let Some(known) = self.covers.get(key) {
            return known.ok_or(CoverError::NotFound);
        }
        let reference = decode_key(key).ok_or(CoverError::BadKey)?;
        let _turn = self
            .cover_runs
            .acquire()
            .await
            .expect("the semaphore is never closed");
        let cover = direct::embedded_cover(&reference, ctx)
            .await?
            .map(|data| Cover {
                data,
                content_type: "image/jpeg",
            });
        self.covers.insert(key.to_owned(), cover.clone());
        cover.ok_or(CoverError::NotFound)
    }
}

/// Gives the tracks of a listing without artwork of their own the listing's, e.g. an album's
/// cover.
pub fn inherit(tracks: &mut [FoundTrack], artwork: Option<&str>) {
    let Some(artwork) = artwork else {
        return;
    };
    for track in tracks.iter_mut().filter(|t| t.info.artwork_url.is_none()) {
        track.info.artwork_url = Some(artwork.to_owned());
    }
}

/// Where the engine serves the cover embedded in the file `reference` points at.
pub fn cover_path(reference: &TrackRef) -> String {
    let json = serde_json::to_vec(reference).expect("references serialize");
    format!("{COVER_PATH}{}", URL_SAFE_NO_PAD.encode(json))
}

fn decode_key(key: &str) -> Option<TrackRef> {
    let json = URL_SAFE_NO_PAD.decode(key).ok()?;
    serde_json::from_slice(&json).ok()
}

async fn fetch_page_image(page: &str, ctx: &MediaContext) -> Option<String> {
    let url = Url::parse(page).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !ctx.config.sources.allow_private_urls {
        net::ensure_public(&url).await.ok()?;
    }
    let mut response = ctx
        .link_client()
        .get(url.clone())
        .timeout(PAGE_TIMEOUT)
        .send()
        .await
        .ok()?;
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("html"));
    if !response.status().is_success() || !is_html {
        return None;
    }
    let mut head = Vec::new();
    while head.len() < PAGE_HEAD_BYTES {
        let Ok(Some(chunk)) = response.chunk().await else {
            break;
        };
        head.extend_from_slice(&chunk);
        if contains_ignoring_case(&head, b"</head>") {
            break;
        }
    }
    meta_image(&String::from_utf8_lossy(&head), &url)
}

/// The preview picture a page's `<meta>` tags declare: `og:image`, else `twitter:image`, as an
/// absolute web URL.
fn meta_image(html: &str, page: &Url) -> Option<String> {
    let mut twitter = None;
    let lower = html.to_ascii_lowercase();
    let mut rest = 0;
    while let Some(start) = lower[rest..].find("<meta").map(|i| rest + i) {
        let end = lower[start..].find('>').map_or(html.len(), |i| start + i);
        rest = end;
        let attributes = attributes(&html[start + "<meta".len()..end]);
        let get = |name: &str| {
            attributes
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let (Some(kind), Some(content)) = (get("property").or_else(|| get("name")), get("content"))
        else {
            continue;
        };
        let Some(url) = page
            .join(&decode_entities(content.trim()))
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
        else {
            continue;
        };
        match kind.to_ascii_lowercase().as_str() {
            "og:image" | "og:image:url" | "og:image:secure_url" => return Some(url.into()),
            "twitter:image" | "twitter:image:src" if twitter.is_none() => {
                twitter = Some(url.into())
            }
            _ => {}
        }
    }
    twitter
}

/// `name="value"`, `name='value'` and `name=value` pairs of a tag.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = tag.trim_start();
    while let Some(equals) = rest.find('=') {
        let name = rest[..equals].split_whitespace().last().unwrap_or_default();
        let after = rest[equals + 1..].trim_start();
        let (value, remainder) = match after.chars().next() {
            Some(quote @ ('"' | '\'')) => match after[1..].find(quote) {
                Some(close) => (&after[1..=close], &after[close + 2..]),
                None => (&after[1..], ""),
            },
            // unquoted values end at whitespace; they may contain slashes, as URLs do
            _ => {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
        };
        found.push((name.to_owned(), value.to_owned()));
        rest = remainder;
    }
    found
}

/// The few entities that appear in URLs in attributes.
fn decode_entities(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn contains_ignoring_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Url {
        Url::parse("https://artist.bandcamp.com/album/x").unwrap()
    }

    #[test]
    fn finds_preview_pictures_in_meta_tags() {
        let html = r#"<html><head>
            <meta name="twitter:image" content="https://cdn.example/t.jpg">
            <META content='/img/cover.jpg?a=1&amp;b=2' PROPERTY="og:image" />
            </head>"#;
        assert_eq!(
            meta_image(html, &page()).as_deref(),
            Some("https://artist.bandcamp.com/img/cover.jpg?a=1&b=2"),
            "og:image wins, resolved against the page, entities decoded"
        );
        let only_twitter = r#"<meta name=twitter:image content=https://cdn.example/t.jpg>"#;
        assert_eq!(
            meta_image(only_twitter, &page()).as_deref(),
            Some("https://cdn.example/t.jpg")
        );
        assert_eq!(
            meta_image("<meta property=\"og:title\" content=\"x\">", &page()),
            None
        );
        assert_eq!(
            meta_image(
                r#"<meta property="og:image" content="javascript:alert(1)">"#,
                &page()
            ),
            None,
            "only web URLs"
        );
        assert_eq!(
            meta_image(
                r#"<meta property="og:image" content="https://x/a.jpg""#,
                &page()
            )
            .as_deref(),
            Some("https://x/a.jpg"),
            "a page cut off mid-tag"
        );
    }

    #[test]
    fn listings_lend_their_artwork_to_tracks_without_any() {
        let track = |artwork: Option<&str>| FoundTrack {
            reference: TrackRef {
                source: "bandcamp".into(),
                uri: "https://artist.bandcamp.com/track/x".into(),
            },
            info: crate::sources::TrackInfo {
                identifier: "x".into(),
                title: "x".into(),
                author: None,
                author_url: None,
                album: None,
                length: None,
                is_stream: false,
                is_seekable: true,
                uri: "https://artist.bandcamp.com/track/x".into(),
                artwork_url: artwork.map(str::to_owned),
                isrc: None,
                source_name: "bandcamp".into(),
            },
        };
        let mut tracks = [track(None), track(Some("https://own.jpg"))];
        inherit(&mut tracks, Some("https://album.jpg"));
        assert_eq!(
            tracks[0].info.artwork_url.as_deref(),
            Some("https://album.jpg")
        );
        assert_eq!(
            tracks[1].info.artwork_url.as_deref(),
            Some("https://own.jpg")
        );
    }

    #[test]
    fn cover_keys_round_trip() {
        let reference = TrackRef {
            source: "local".into(),
            uri: "/music/Some Album/01 - Track.flac".into(),
        };
        let path = cover_path(&reference);
        let key = path.strip_prefix(COVER_PATH).unwrap();
        assert!(
            key.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(decode_key(key), Some(reference));
        assert_eq!(decode_key("not a key!"), None);
    }
}
