//! Producers deliver a track's container bytes.
//!
//! A producer is always read forward-only; seeking happens on the packet timeline. A producer
//! that can start near an arbitrary time (`ProducerCaps::time_seek`) lets the player re-open the
//! media far ahead of the downloaded data instead of waiting for it.

use std::io;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;

use crate::timeline::Ticks;

pub mod ffmpeg;
pub mod file;
pub mod growing_file;
pub mod process;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampMode {
    /// Container timestamps are already positions in the media.
    Absolute,
    /// Container timestamps restart at zero; this offset is added to them.
    RelativeTo(Ticks),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProducerCaps {
    /// `open(Some(t))` delivers media starting at or shortly before `t`.
    pub time_seek: bool,
    pub live: bool,
}

pub struct ProducerStream {
    pub bytes: BoxStream<'static, io::Result<Bytes>>,
    pub timestamps: TimestampMode,
    /// File extension helping the demuxer probe, e.g. `webm`.
    pub hint: Option<String>,
}

impl std::fmt::Debug for ProducerStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProducerStream")
            .field("timestamps", &self.timestamps)
            .field("hint", &self.hint)
            .finish_non_exhaustive()
    }
}

pub trait Producer: Send + Sync + 'static {
    fn caps(&self) -> ProducerCaps;

    /// Opens the media at `start` (best effort unless `caps().time_seek`), or at the beginning.
    fn open(&self, start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>>;
}

/// Formats seconds the way ffmpeg and yt-dlp accept them.
pub(crate) fn seconds_arg(t: Ticks) -> String {
    format!(
        "{:.3}",
        t.max(0) as f64 / crate::timeline::TICKS_PER_SECOND as f64
    )
}
