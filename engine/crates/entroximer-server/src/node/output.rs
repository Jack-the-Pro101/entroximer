//! Feeds a guild's mixer to songbird: one input per guild that never ends.
//!
//! songbird's mixer reads it on its own thread, 20 ms at a time. The mixer never blocks (missing
//! media renders as silence), and its events are handed to the player's event task.

use std::io::{self, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use entroximer_core::dsp::CHANNELS;
use entroximer_core::mixer::Mixer;
use songbird::input::core::io::MediaSource;
use tokio::sync::mpsc::UnboundedSender;

use super::player::Signal;

/// Discord's frame: 20 ms at 48 kHz.
const FRAMES_PER_FILL: usize = 960;

/// Interleaved little-endian f32 stereo, as songbird's `RawAdapter` expects.
pub(super) struct MixerSource {
    mixer: Arc<Mutex<Mixer>>,
    signals: UnboundedSender<Signal>,
    samples: Vec<f32>,
    bytes: Vec<u8>,
    read: usize,
    /// Renders that panicked; the first is logged.
    panics: u64,
}

impl MixerSource {
    pub(super) fn new(mixer: Arc<Mutex<Mixer>>, signals: UnboundedSender<Signal>) -> Self {
        Self {
            mixer,
            signals,
            samples: vec![0.0; FRAMES_PER_FILL * CHANNELS],
            bytes: Vec::with_capacity(FRAMES_PER_FILL * CHANNELS * size_of::<f32>()),
            read: 0,
            panics: 0,
        }
    }

    fn fill(&mut self) {
        // songbird mixes several guilds on one thread: a panic here must not take them all down
        let rendered = catch_unwind(AssertUnwindSafe(|| {
            let mut mixer = self.mixer.lock().unwrap_or_else(|e| e.into_inner());
            mixer.render(&mut self.samples);
            mixer.drain_events()
        }));
        let events = rendered.unwrap_or_else(|_| {
            if self.panics == 0 {
                tracing::error!("rendering a guild's audio panicked; it plays silence instead");
            }
            self.panics += 1;
            self.samples.fill(0.0);
            Vec::new()
        });
        for event in events {
            // the receiver only goes away with the player, which also stops this input
            let _ = self.signals.send(Signal::Deck(event));
        }
        self.bytes.clear();
        self.bytes
            .extend(self.samples.iter().flat_map(|s| s.to_le_bytes()));
        self.read = 0;
    }
}

impl Read for MixerSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.read == self.bytes.len() {
            self.fill();
        }
        let n = buf.len().min(self.bytes.len() - self.read);
        buf[..n].copy_from_slice(&self.bytes[self.read..self.read + n]);
        self.read += n;
        Ok(n)
    }
}

impl Seek for MixerSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a mixer is a live output; seek its tracks instead",
        ))
    }
}

impl MediaSource for MixerSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}
