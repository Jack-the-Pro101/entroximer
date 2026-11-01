//! Media core of the Entroximer engine.
//!
//! - [`producer`]: where a track's bytes come from (yt-dlp pipes, growing files, ffmpeg).
//! - [`timeline`]: the time-indexed packet cache all seeking happens on, spilled to disk.
//! - [`demux`] / [`decode`] / [`resample`]: bytes to packets to 48 kHz stereo.
//! - [`dsp`]: post-processing node graphs.
//! - [`player`]: ties it together into seekable, processed PCM.

pub mod decode;
pub mod demux;
pub mod dsp;
pub mod error;
pub mod player;
pub mod producer;
pub mod resample;
pub mod timeline;
pub mod transport;
