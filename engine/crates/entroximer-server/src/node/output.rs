//! Feeds a deck to songbird: one input per guild that never ends.
//!
//! songbird's mixer reads it on its own thread, 20 ms at a time. The deck never blocks (missing
//! media renders as silence), and its events are handed to the player's event task.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use entroximer_core::deck::Deck;
use songbird::input::core::io::MediaSource;
use tokio::sync::mpsc::UnboundedSender;

use super::player::Signal;

/// Discord's frame: 20 ms at 48 kHz.
const FRAMES_PER_FILL: usize = 960;
const CHANNELS: usize = 2;

/// Interleaved little-endian f32 stereo, as songbird's `RawAdapter` expects.
pub(super) struct DeckSource {
    deck: Arc<Mutex<Deck>>,
    signals: UnboundedSender<Signal>,
    samples: Vec<f32>,
    bytes: Vec<u8>,
    read: usize,
}

impl DeckSource {
    pub(super) fn new(deck: Arc<Mutex<Deck>>, signals: UnboundedSender<Signal>) -> Self {
        Self {
            deck,
            signals,
            samples: vec![0.0; FRAMES_PER_FILL * CHANNELS],
            bytes: Vec::with_capacity(FRAMES_PER_FILL * CHANNELS * 4),
            read: 0,
        }
    }

    fn fill(&mut self) {
        let events = {
            let mut deck = self.deck.lock().unwrap_or_else(|e| e.into_inner());
            deck.render(&mut self.samples);
            deck.drain_events()
        };
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

impl Read for DeckSource {
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

impl Seek for DeckSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a deck is a live output; seek its tracks instead",
        ))
    }
}

impl MediaSource for DeckSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}
