//! Tracks resolved and downloaded with yt-dlp (YouTube and the many other sites it supports).
//!
//! Resolution runs `yt-dlp -J` once and the resulting info JSON is fed back with
//! `--load-info-json -` for every download, so a page is never extracted twice. The chosen audio
//! format's protocol decides how its bytes arrive:
//!
//! - `https`: yt-dlp writes the file to stdout. Re-opening at an offset goes through ffmpeg,
//!   which seeks the format's direct URL with range requests using the container index.
//! - `sabr`: YouTube's server-driven streaming (yt-dlp PR #13515) cannot be written to stdout,
//!   so yt-dlp writes a file that is read while it grows. Seeking waits for the download.
//! - `m3u8`/`m3u8_native` (live streams): ffmpeg remuxes the playlist to Matroska; yt-dlp's own
//!   stdout output would be MPEG-TS, which the demuxer does not read.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use entroximer_core::producer::ffmpeg::FfmpegProducer;
use entroximer_core::producer::growing_file::GrowingFileProducer;
use entroximer_core::producer::process::{ProcessProducer, ProcessSpec};
use entroximer_core::producer::{Producer, ProducerCaps, ProducerStream, TimestampMode};
use entroximer_core::timeline::Ticks;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;

use super::{ResolveError, ResolvedTrack, TrackInfo};
use crate::config::MediaContext;

pub(super) async fn resolve(
    query: &str,
    ctx: &MediaContext,
) -> Result<ResolvedTrack, ResolveError> {
    ctx.ytdlp
        .ready()
        .await
        .map_err(ResolveError::YtDlpUnavailable)?;
    let output = tokio::process::Command::new(ctx.ytdlp.program())
        .args(&ctx.config.ytdlp.args.0)
        .args(["-J", "--no-playlist", "--no-warnings", "--", query])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(ResolveError::Spawn)?;
    if !output.status.success() {
        return Err(ResolveError::Extraction {
            query: query.to_owned(),
            message: last_line(&output.stderr),
        });
    }
    let mut json: Value = serde_json::from_slice(&output.stdout).map_err(ResolveError::Json)?;
    // searches (`ytsearch:...`) return a playlist of fully extracted entries
    if json.get("_type").and_then(Value::as_str) == Some("playlist") {
        json = json
            .get_mut("entries")
            .and_then(|entries| entries.as_array_mut())
            .filter(|entries| !entries.is_empty())
            .map(|entries| entries.swap_remove(0))
            .ok_or_else(|| ResolveError::NoResults(query.to_owned()))?;
    }
    let info = Info::deserialize(&json).map_err(ResolveError::Json)?;
    let format = info
        .best_audio()
        .ok_or_else(|| ResolveError::NoAudio(query.to_owned()))?;
    let track = info.track_info(query);
    tracing::debug!(
        title = %track.title,
        format = ?format.format_id,
        protocol = ?format.protocol,
        "resolved with yt-dlp"
    );
    // googlevideo refuses requests for about two seconds after issuing their URLs (observed
    // November 2026); hold our own requests back instead of failing and retrying
    let urls_valid_from = Instant::now()
        + if info.is_youtube() {
            YOUTUBE_URL_ACTIVATION
        } else {
            Duration::ZERO
        };
    let producer = YtDlpProducer::new(
        ctx,
        Bytes::from(json.to_string()),
        &format,
        info.live(),
        urls_valid_from,
    );
    Ok(ResolvedTrack {
        info: track,
        producer: Arc::new(producer),
    })
}

const YOUTUBE_URL_ACTIVATION: Duration = Duration::from_millis(2500);

/// The parts of yt-dlp's info JSON we use; the rest is passed back to yt-dlp untouched.
#[derive(Debug, Deserialize)]
struct Info {
    id: Option<String>,
    title: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    duration: Option<f64>,
    is_live: Option<bool>,
    webpage_url: Option<String>,
    thumbnail: Option<String>,
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
}

impl Format {
    fn is_audio_only(&self) -> bool {
        self.vcodec.as_deref() == Some("none") && self.acodec.as_deref() != Some("none")
    }

    fn transport(&self) -> Option<Transport> {
        match self.protocol.as_deref()? {
            "https" | "http" => Some(Transport::Stdout),
            "sabr" => Some(Transport::GrowingFile),
            "m3u8" | "m3u8_native" => Some(Transport::Hls),
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

impl Info {
    fn is_youtube(&self) -> bool {
        self.extractor_key.as_deref() == Some("Youtube")
    }

    fn live(&self) -> bool {
        self.is_live.unwrap_or(false)
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

    fn track_info(&self, query: &str) -> TrackInfo {
        TrackInfo {
            identifier: self.id.clone().unwrap_or_else(|| query.to_owned()),
            title: self.title.clone().unwrap_or_else(|| "Unknown title".into()),
            author: self
                .uploader
                .clone()
                .or_else(|| self.channel.clone())
                .unwrap_or_else(|| "Unknown author".into()),
            length_ms: self.duration.map(|d| (d * 1000.0) as u64),
            is_stream: self.live(),
            uri: self.webpage_url.clone().unwrap_or_else(|| query.to_owned()),
            artwork_url: self.thumbnail.clone(),
            source_name: self
                .extractor_key
                .as_deref()
                .unwrap_or("yt-dlp")
                .to_lowercase(),
        }
    }
}

/// Downloads with yt-dlp, and re-opens `https` formats at an offset through ffmpeg.
struct YtDlpProducer {
    download: Arc<dyn Producer>,
    seek: Option<FfmpegProducer>,
    live: bool,
    /// Direct requests to the format's URL are not made before this.
    urls_valid_from: Instant,
}

impl YtDlpProducer {
    fn new(
        ctx: &MediaContext,
        info_json: Bytes,
        format: &Format,
        live: bool,
        urls_valid_from: Instant,
    ) -> Self {
        let media = &ctx.config;
        let transport = format
            .transport()
            .expect("best_audio only picks supported protocols");
        let direct = format.url.as_ref().map(|url| FfmpegProducer {
            ffmpeg: media.ffmpeg.clone(),
            input: url.clone(),
            headers: format
                .http_headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        });
        let caps = ProducerCaps {
            time_seek: false,
            live,
        };
        let hint = format.ext.clone();
        let base_args = || -> Vec<String> {
            let mut args = media.ytdlp.args.0.clone();
            args.extend(
                [
                    "--load-info-json",
                    "-",
                    "--quiet",
                    "--no-warnings",
                    "--no-progress",
                ]
                .map(String::from),
            );
            if let Some(id) = &format.format_id {
                args.extend(["-f".to_owned(), id.clone()]);
            }
            args
        };

        let download: Arc<dyn Producer> = match transport {
            Transport::Stdout => {
                let spec = ProcessSpec::new(ctx.ytdlp.program())
                    .args(base_args())
                    .args(["-o", "-"])
                    .stdin(info_json);
                Arc::new(ProcessProducer::new(caps, hint, move |_| {
                    (spec.clone(), TimestampMode::Absolute)
                }))
            }
            Transport::GrowingFile => {
                let (program, args) = (ctx.ytdlp.program(), base_args());
                Arc::new(GrowingFileProducer::new(
                    ctx.work_dir().to_owned(),
                    caps,
                    hint,
                    move |_, dir: &Path| {
                        let template = dir.join("media.%(ext)s");
                        let spec = ProcessSpec::new(&program)
                            .args(args.clone())
                            .arg("-o")
                            .arg(template.to_string_lossy())
                            .stdin(info_json.clone());
                        (spec, TimestampMode::Absolute)
                    },
                ))
            }
            Transport::Hls => Arc::new(LiveFfmpeg(
                direct
                    .clone()
                    .expect("HLS formats always carry their playlist URL"),
            )),
        };
        // live streams cannot seek; SABR has no URL to seek with
        let seek = direct.filter(|_| transport == Transport::Stdout && !live);
        Self {
            download,
            seek,
            live,
            urls_valid_from,
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
        match (start.filter(|&t| t > 0), &self.seek) {
            (Some(start), Some(seek)) => {
                // ffmpeg is only spawned once the returned future runs
                let (opening, valid_from) = (seek.open(Some(start)), self.urls_valid_from);
                Box::pin(async move {
                    tokio::time::sleep_until(valid_from.into()).await;
                    opening.await
                })
            }
            _ => self.download.open(None),
        }
    }
}

/// ffmpeg on a live playlist: never seeks.
struct LiveFfmpeg(FfmpegProducer);

impl Producer for LiveFfmpeg {
    fn caps(&self) -> ProducerCaps {
        ProducerCaps {
            time_seek: false,
            live: true,
        }
    }

    fn open(&self, _start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        self.0.open(None)
    }
}

fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no error output")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn info(formats: Value) -> Info {
        Info::deserialize(json!({ "id": "x", "formats": formats })).unwrap()
    }

    fn audio(id: &str, acodec: &str, abr: f64, protocol: &str) -> Value {
        json!({ "format_id": id, "acodec": acodec, "vcodec": "none", "abr": abr, "protocol": protocol, "url": "https://example.invalid" })
    }

    #[test]
    fn prefers_opus_full_range_seekable_then_bitrate() {
        let i = info(json!([
            audio("140", "mp4a.40.2", 129.0, "https"),
            audio("251-drc", "opus", 107.0, "https"),
            audio("251", "opus", 106.0, "https"),
            audio("250", "opus", 60.0, "https"),
            { "format_id": "18", "acodec": "mp4a", "vcodec": "avc1", "protocol": "https" },
        ]));
        assert_eq!(i.best_audio().unwrap().format_id.as_deref(), Some("251"));

        let https_beats_higher_bitrate_sabr = info(json!([
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

        let sabr_only = info(json!([audio("251", "opus", 160.0, "sabr")]));
        assert_eq!(
            sabr_only.best_audio().unwrap().transport(),
            Some(Transport::GrowingFile)
        );

        assert!(
            info(json!([audio("x", "opus", 1.0, "f4m")]))
                .best_audio()
                .is_none()
        );
    }
}
