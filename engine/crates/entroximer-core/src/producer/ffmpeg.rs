//! Time-seekable producer backed by ffmpeg.
//!
//! `-ss` before `-i` seeks with the container index (WebM Cues, MP4 sidx/moov) using HTTP range
//! requests or plain file reads, and `-copyts` keeps the original timestamps, so a range opened
//! mid-media lines up exactly with what is already buffered. Audio is copied, never re-encoded.

use std::io;

use futures_util::future::BoxFuture;

use super::process::{ProcessSpec, stdout_stream};
use super::{Producer, ProducerCaps, ProducerStream, TimestampMode, seconds_arg};
use crate::timeline::Ticks;

#[derive(Clone, Debug)]
pub struct FfmpegProducer {
    pub ffmpeg: String,
    /// A URL or a local path.
    pub input: String,
    /// HTTP headers to send, e.g. the ones yt-dlp reports for a format.
    pub headers: Vec<(String, String)>,
}

impl FfmpegProducer {
    pub fn new(ffmpeg: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            input: input.into(),
            headers: Vec::new(),
        }
    }

    pub fn spec(&self, start: Option<Ticks>) -> ProcessSpec {
        let mut spec =
            ProcessSpec::new(&self.ffmpeg).args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
        if !self.headers.is_empty() {
            let headers: String = self
                .headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}\r\n"))
                .collect();
            spec = spec.arg("-headers").arg(headers);
        }
        if let Some(start) = start.filter(|&t| t > 0) {
            spec = spec.arg("-ss").arg(seconds_arg(start));
        }
        spec.arg("-copyts")
            .arg("-i")
            .arg(&self.input)
            .args(["-map", "0:a:0", "-c", "copy", "-f", "matroska", "pipe:1"])
    }
}

impl Producer for FfmpegProducer {
    fn caps(&self) -> ProducerCaps {
        ProducerCaps {
            time_seek: true,
            live: false,
        }
    }

    fn open(&self, start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        let spec = self.spec(start);
        Box::pin(async move {
            Ok(ProducerStream {
                bytes: stdout_stream(&spec)?,
                timestamps: TimestampMode::Absolute,
                hint: Some("mkv".into()),
            })
        })
    }
}
