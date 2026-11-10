//! Reads back from disk what a player is about to play.
//!
//! A timeline holds in RAM only what was downloaded near its playheads; the rest of a long
//! track, and all of one kept for playing later, is on disk by the time it plays. Rendering
//! would read each such block back as it reaches it, on the audio thread. This task follows the
//! player's reader instead and has the blocks ahead of it read on the runtime's blocking
//! threads, so that rendering finds them in RAM. Rendering is left with the reads nothing can
//! foresee: those right after a seek.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::display_chain;
use crate::timeline::{ReaderId, TICKS_PER_SECOND, Ticks, Timeline};

/// How far past the playhead blocks are read back: several of them, so that the reads are done
/// long before playback gets there, even on a busy host.
const AHEAD: Ticks = 5 * TICKS_PER_SECOND;
/// How far before the playhead: a seek decodes this much before its target, as preroll.
const BEHIND: Ticks = TICKS_PER_SECOND / 10;
/// Playback that has moved on this far, a block's length, has more read back.
pub(super) const STEP: Ticks = TICKS_PER_SECOND;

/// The task reading back what one reader of a timeline plays next; it ends when dropped.
pub(super) struct ReadAhead {
    moved: Arc<Notify>,
    task: JoinHandle<()>,
}

impl ReadAhead {
    /// Starts with what `reader` plays first.
    pub(super) fn spawn(rt: &Handle, timeline: Arc<Timeline>, reader: ReaderId) -> Self {
        let moved = Arc::new(Notify::new());
        let task = rt.spawn(follow(timeline, reader, Arc::clone(&moved)));
        Self { moved, task }
    }

    /// The reader's playhead has moved: there may be more to read back. This only wakes the
    /// task, so it is fine on the audio thread.
    pub(super) fn moved(&self) {
        self.moved.notify_one();
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn follow(timeline: Arc<Timeline>, reader: ReaderId, moved: Arc<Notify>) {
    loop {
        let spilled = timeline.spilled_around(reader, BEHIND, AHEAD);
        if !spilled.is_empty() {
            let timeline = Arc::clone(&timeline);
            match tokio::task::spawn_blocking(move || timeline.load(spilled)).await {
                Ok(Ok(())) => {}
                // rendering reads the block itself, and playback fails with the error if it
                // is still there
                Ok(Err(e)) => {
                    tracing::debug!(error = %display_chain(&e), "could not read cached media ahead");
                }
                // the runtime is shutting down
                Err(_) => return,
            }
        }
        moved.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bytes::Bytes;

    use super::*;
    use crate::timeline::{Finish, Packet, RetentionConfig, SpillConfig};

    const PACKET: Ticks = 960;

    /// A timeline holding `seconds` of media on disk and none of it in RAM.
    fn spilled(dir: &tempfile::TempDir, seconds: i64) -> Arc<Timeline> {
        let timeline = Timeline::new(RetentionConfig {
            spill: Some(SpillConfig {
                dir: dir.path().to_owned(),
                ..SpillConfig::default()
            }),
            ..RetentionConfig::default()
        });
        let producer = timeline.register_producer(None);
        for i in 0..seconds * TICKS_PER_SECOND / PACKET {
            let packet = Packet {
                pts: i * PACKET,
                dur: PACKET,
                trim_start: 0,
                trim_end: 0,
                data: Bytes::from(vec![0; 400]),
            };
            timeline.push(producer, packet);
        }
        timeline.finish(producer, Finish::Eof);
        timeline.park(0, None).expect("all of it is on disk");
        assert_eq!(timeline.stats().memory_bytes, 0);
        Arc::new(timeline)
    }

    /// Waits until nothing `reader` plays next is left to read back.
    fn wait_until_read(timeline: &Timeline, reader: ReaderId) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !timeline.spilled_around(reader, BEHIND, AHEAD).is_empty() {
            assert!(Instant::now() < deadline, "nothing was read ahead");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn what_plays_next_is_in_ram_before_playback_reaches_it() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let timeline = spilled(&dir, 60);
        let reader = timeline.add_reader(0);
        let read_ahead = ReadAhead::spawn(rt.handle(), Arc::clone(&timeline), reader);

        // from a thread that is not the runtime's, as rendering is
        wait_until_read(&timeline, reader);
        for second in 0..20 {
            let position = second * TICKS_PER_SECOND;
            timeline.set_playhead(reader, position);
            read_ahead.moved();
            wait_until_read(&timeline, reader);
            for packet in 0..TICKS_PER_SECOND / PACKET {
                timeline.lookup(position + packet * PACKET).unwrap();
            }
        }
        assert_eq!(timeline.stats().disk_lookups, 0);

        // a seek is not foreseen: its first block is read where it is looked up
        timeline.set_playhead(reader, 50 * TICKS_PER_SECOND);
        timeline.lookup(50 * TICKS_PER_SECOND).unwrap();
        assert_eq!(timeline.stats().disk_lookups, 1);
    }
}
