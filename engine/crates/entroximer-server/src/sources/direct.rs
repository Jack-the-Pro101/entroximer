//! Media that ffmpeg plays directly: `http` for links to media files and internet radio, and
//! `local` for files on the engine's host. Their metadata comes from the files' own tags, read
//! with ffprobe.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entroximer_core::producer::ffmpeg::FfmpegProducer;
use reqwest::Url;
use reqwest::header::{CONTENT_TYPE, HeaderMap, RANGE};
use serde::Deserialize;

use super::provider::{Provider, ProviderDescriptor};
use super::{FoundTrack, LoadOutcome, ResolveError, ResolvedTrack, TrackInfo, TrackRef, net};
use crate::api::protocol::Millis;
use crate::config::MediaContext;

/// Paths ending like this are media even if the server says otherwise.
const MEDIA_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "wav", "m4a", "aac", "webm", "mka", "mkv", "mp4", "m3u8",
];
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) struct HttpProvider {
    descriptor: ProviderDescriptor,
}

impl HttpProvider {
    pub(super) fn new() -> Self {
        Self {
            descriptor: descriptor("http", "Direct link"),
        }
    }
}

#[async_trait]
impl Provider for HttpProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn accepts(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
    }

    async fn load_url(
        &self,
        url: &Url,
        ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError> {
        if !ctx.config.sources.allow_private_urls {
            net::ensure_public(url).await?;
        }
        let by_extension = has_media_extension(url);
        let head = match Head::fetch(url).await {
            Ok(head) => head,
            // perhaps a page that yt-dlp makes sense of
            Err(_) if !by_extension => return Ok(None),
            Err(e) => return Err(e),
        };
        if !(by_extension || head.is_media()) {
            return Ok(None);
        }
        let track = describe(url.as_str(), "http", Some(&head), ctx).await?;
        Ok(Some(LoadOutcome::Track(track)))
    }

    async fn resolve(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let uri = &track.reference.uri;
        let url = Url::parse(uri).map_err(|_| ResolveError::Unsupported(uri.clone()))?;
        if !ctx.config.sources.allow_private_urls {
            net::ensure_public(&url).await?;
        }
        let head = Head::fetch(&url).await?;
        describe(url.as_str(), "http", Some(&head), ctx).await
    }
}

/// Files on the engine's host; only registered when the operator allows them.
pub(super) struct LocalProvider {
    descriptor: ProviderDescriptor,
}

impl LocalProvider {
    pub(super) fn new() -> Self {
        Self {
            descriptor: descriptor("local", "Local file"),
        }
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn accepts(&self, url: &Url) -> bool {
        url.scheme() == "file"
    }

    async fn load_url(
        &self,
        url: &Url,
        ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError> {
        let path = url
            .to_file_path()
            .map_err(|()| ResolveError::Unsupported(url.to_string()))?;
        let track = describe(&path.to_string_lossy(), "local", None, ctx).await?;
        Ok(Some(LoadOutcome::Track(track)))
    }

    async fn resolve(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let path = &track.reference.uri;
        if !Path::new(path).is_file() {
            return Err(ResolveError::Unsupported(path.clone()));
        }
        describe(path, "local", None, ctx).await
    }
}

fn descriptor(name: &str, display_name: &str) -> ProviderDescriptor {
    ProviderDescriptor {
        name: name.to_owned(),
        display_name: display_name.to_owned(),
        search_prefixes: Vec::new(),
        hosts: Vec::new(),
    }
}

/// What the server says about a link before anything is downloaded.
#[derive(Debug)]
struct Head {
    content_type: Option<String>,
    /// Internet radio: no length, or Icecast/Shoutcast headers.
    live: bool,
    /// The radio station's name.
    station: Option<String>,
}

impl Head {
    async fn fetch(url: &Url) -> Result<Self, ResolveError> {
        let response = net::client()
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|source| ResolveError::Http {
                url: url.to_string(),
                source,
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ResolveError::HttpStatus {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }
        let headers = response.headers();
        let header = |name: &str| text(headers, name);
        let icy = headers.keys().any(|name| name.as_str().starts_with("icy-"));
        // a file answers a range request with 206, or with 200 and its length
        let endless = status.as_u16() == 200 && response.content_length().is_none();
        Ok(Self {
            content_type: header(CONTENT_TYPE.as_str()),
            live: icy || endless,
            station: header("icy-name"),
        })
    }

    fn is_media(&self) -> bool {
        self.content_type.as_deref().is_some_and(|content_type| {
            let essence = content_type.split(';').next().unwrap_or_default().trim();
            essence.starts_with("audio/")
                || essence.starts_with("video/")
                || matches!(
                    essence,
                    "application/ogg" | "application/vnd.apple.mpegurl" | "application/x-mpegurl"
                )
        })
    }
}

fn text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn has_media_extension(url: &Url) -> bool {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(_, extension)| {
            MEDIA_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// Reads `input`'s metadata with ffprobe and prepares ffmpeg to play it.
async fn describe(
    input: &str,
    source: &str,
    head: Option<&Head>,
    ctx: &MediaContext,
) -> Result<ResolvedTrack, ResolveError> {
    let probe = Probe::run(input, ctx).await?;
    if !probe.has_audio() {
        return Err(ResolveError::NoAudio(input.to_owned()));
    }
    let length = probe.duration();
    let live = head.is_some_and(|head| head.live) || length.is_none();
    let info = TrackInfo {
        identifier: input.to_owned(),
        title: probe
            .tag("title")
            .or_else(|| head.and_then(|head| head.station.clone()))
            .unwrap_or_else(|| title_from(input)),
        author: probe.tag("artist").or_else(|| probe.tag("album_artist")),
        author_url: None,
        album: probe.tag("album"),
        length: length.filter(|_| !live),
        is_stream: live,
        is_seekable: !live,
        uri: input.to_owned(),
        artwork_url: None,
        isrc: probe.tag("isrc").or_else(|| probe.tag("tsrc")),
        source_name: source.to_owned(),
    };
    let producer = FfmpegProducer {
        live,
        ..FfmpegProducer::new(&ctx.config.ffmpeg, input)
    };
    Ok(ResolvedTrack {
        track: FoundTrack {
            reference: TrackRef {
                source: source.to_owned(),
                uri: input.to_owned(),
            },
            info,
        },
        producer: Arc::new(producer),
    })
}

/// The file name without its extension, e.g. `My Song` for `https://host/music/My%20Song.mp3`.
fn title_from(input: &str) -> String {
    let name = match Url::parse(input) {
        Ok(url) => url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .map(percent_decode)
            .unwrap_or_default(),
        Err(_) => Path::new(input)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let stem = name
        .rsplit_once('.')
        .map_or(name.as_str(), |(stem, _)| stem);
    if stem.is_empty() {
        input.to_owned()
    } else {
        stem.to_owned()
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                i += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// ffprobe's view of a file: container duration and tags, and the streams.
#[derive(Debug, Default, Deserialize)]
struct Probe {
    #[serde(default)]
    format: ProbeFormat,
    #[serde(default)]
    streams: Vec<ProbeStream>,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

impl Probe {
    async fn run(input: &str, ctx: &MediaContext) -> Result<Self, ResolveError> {
        let program = &ctx.config.ffprobe;
        let probing = tokio::process::Command::new(program)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                "-i",
            ])
            .arg(input)
            .kill_on_drop(true)
            .output();
        let failed = |message: String| ResolveError::Probe {
            target: input.to_owned(),
            message,
        };
        let output = tokio::time::timeout(PROBE_TIMEOUT, probing)
            .await
            .map_err(|_| failed("timed out".into()))?
            .map_err(|source| ResolveError::Spawn("ffprobe", source))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.lines().last().unwrap_or("no error output").trim();
            return Err(failed(message.to_owned()));
        }
        serde_json::from_slice(&output.stdout).map_err(|e| ResolveError::Json("ffprobe", e))
    }

    fn has_audio(&self) -> bool {
        self.streams
            .iter()
            .any(|s| s.codec_type.as_deref() == Some("audio"))
    }

    fn duration(&self) -> Option<Millis> {
        self.format
            .duration
            .as_deref()
            .and_then(|d| d.parse::<f64>().ok())
            .and_then(Millis::from_secs_f64)
    }

    /// A tag by name, ignoring case: from the container, else from the audio stream (where Ogg
    /// files keep theirs).
    fn tag(&self, name: &str) -> Option<String> {
        let audio = self
            .streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("audio"));
        std::iter::once(&self.format.tags)
            .chain(audio.map(|s| &s.tags))
            .find_map(|tags| {
                tags.iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim().to_owned())
            })
            .filter(|value| !value.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_fall_back_to_the_file_name() {
        assert_eq!(
            title_from("https://host/music/My%20Song.mp3?x=1"),
            "My Song"
        );
        assert_eq!(title_from("/srv/music/track.flac"), "track");
        assert_eq!(
            title_from("https://radio.example/"),
            "https://radio.example/"
        );
    }

    #[test]
    fn recognises_media_links() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert!(has_media_extension(&url("https://h/a/b.MP3")));
        assert!(!has_media_extension(&url("https://h/watch?v=mp3")));
        let head = |content_type: &str| Head {
            content_type: Some(content_type.into()),
            live: false,
            station: None,
        };
        assert!(head("audio/mpeg").is_media());
        assert!(head("application/ogg; charset=binary").is_media());
        assert!(!head("text/html; charset=utf-8").is_media());
    }

    #[test]
    fn tags_are_found_in_the_container_or_the_stream() {
        let probe: Probe = serde_json::from_value(serde_json::json!({
            "format": { "duration": "3.000000", "tags": { "TITLE": "Container title" } },
            "streams": [{ "codec_type": "audio", "tags": { "ARTIST": "Stream artist" } }],
        }))
        .unwrap();
        assert_eq!(probe.tag("title").as_deref(), Some("Container title"));
        assert_eq!(probe.tag("artist").as_deref(), Some("Stream artist"));
        assert_eq!(probe.duration(), Some(Millis(3000)));
        assert!(probe.has_audio());
    }
}
