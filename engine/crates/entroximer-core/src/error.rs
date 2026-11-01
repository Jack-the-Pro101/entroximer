//! Error types shared across the media pipeline.

use std::error::Error;
use std::fmt;
use std::io;

use symphonia::core::audio::Channels;
use symphonia::core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia::core::errors::Error as SymphoniaError;

/// Why a producer failed to deliver its media.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// The producer itself failed: it could not start, or its stream broke off.
    #[error("the media source failed")]
    Source(#[source] io::Error),
    #[error("unsupported or corrupt media")]
    Probe(#[source] SymphoniaError),
    #[error("media has no audio track")]
    NoAudioTrack,
    #[error("audio track has no {0}")]
    IncompleteTrack(&'static str),
    #[error(transparent)]
    CodecMismatch(#[from] CodecMismatch),
    #[error("failed to read media")]
    Demux(#[source] SymphoniaError),
    #[error("demuxer panicked")]
    Panicked,
}

/// A producer delivered a different codec than the one the timeline was started with.
#[derive(Debug, thiserror::Error)]
#[error("a producer delivered {found}, but the timeline holds {expected}")]
pub struct CodecMismatch {
    pub expected: CodecSummary,
    pub found: CodecSummary,
}

/// The parts of codec parameters that must match between producers of one timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecSummary {
    pub codec: AudioCodecId,
    pub sample_rate: Option<u32>,
    pub channels: Option<Channels>,
}

impl From<&AudioCodecParameters> for CodecSummary {
    fn from(params: &AudioCodecParameters) -> Self {
        Self {
            codec: params.codec,
            sample_rate: params.sample_rate,
            channels: params.channels.clone(),
        }
    }
}

impl fmt::Display for CodecSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "codec {:?}", self.codec)?;
        if let Some(rate) = self.sample_rate {
            write!(f, " at {rate} Hz")?;
        }
        if let Some(channels) = &self.channels {
            write!(f, " with {} channels", channels.count())?;
        }
        Ok(())
    }
}

/// Renders an error and all of its sources as `outer: inner: innermost`, for logs and clients.
pub fn display_chain(err: &(dyn Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        out.push_str(": ");
        out.push_str(&err.to_string());
        source = err.source();
    }
    out
}
