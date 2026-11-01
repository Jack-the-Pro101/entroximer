//! Turning user input (URLs, searches, local paths) into playable tracks.

mod ytdlp;

use std::io;
use std::path::Path;
use std::sync::Arc;

use entroximer_core::producer::Producer;
use entroximer_core::producer::ffmpeg::FfmpegProducer;
use serde::Serialize;

use crate::config::MediaContext;
use crate::managed_ytdlp::InstallError;

/// What clients are told about a track.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub identifier: String,
    pub title: String,
    pub author: String,
    pub length_ms: Option<u64>,
    pub is_stream: bool,
    pub uri: String,
    pub artwork_url: Option<String>,
    pub source_name: String,
}

pub struct ResolvedTrack {
    pub info: TrackInfo,
    pub producer: Arc<dyn Producer>,
}

impl std::fmt::Debug for ResolvedTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedTrack")
            .field("info", &self.info)
            .field("caps", &self.producer.caps())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("yt-dlp is unavailable")]
    YtDlpUnavailable(#[source] InstallError),
    #[error("failed to run yt-dlp")]
    Spawn(#[source] io::Error),
    #[error("yt-dlp could not load `{query}`: {message}")]
    Extraction { query: String, message: String },
    #[error("yt-dlp returned unexpected JSON")]
    Json(#[source] serde_json::Error),
    #[error("no results for `{0}`")]
    NoResults(String),
    #[error("`{0}` has no playable audio format")]
    NoAudio(String),
}

/// Resolves an existing local path through ffmpeg and everything else through yt-dlp.
pub async fn resolve(input: &str, ctx: &MediaContext) -> Result<ResolvedTrack, ResolveError> {
    let path = Path::new(input);
    if path.is_file() {
        let name = path
            .file_name()
            .map_or_else(|| input.to_owned(), |n| n.to_string_lossy().into_owned());
        return Ok(ResolvedTrack {
            info: TrackInfo {
                identifier: input.to_owned(),
                title: name,
                author: "Local file".into(),
                length_ms: None,
                is_stream: false,
                uri: input.to_owned(),
                artwork_url: None,
                source_name: "local".into(),
            },
            producer: Arc::new(FfmpegProducer::new(&ctx.config.ffmpeg, input)),
        });
    }
    ytdlp::resolve(input, ctx).await
}
