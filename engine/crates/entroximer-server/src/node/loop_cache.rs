//! Tracks of repeated queues kept on disk once they have played, so that the queue's next round
//! plays them from there instead of loading them again: no yt-dlp run and no download, which
//! matters most for YouTube, which flags addresses that ask it for too much.
//!
//! Only tracks whose whole media is on disk are kept, together at most `ENTROXIMER_LOOP_CACHE_MB`
//! (shared by every player, apart from what the tracks playing may take). When that is full, the
//! cache keeps what it has: a repeated queue plays its tracks in a cycle, so making room by age
//! would drop each track just before it comes round again, and a queue too long for the cache
//! would gain nothing at all. The one exception is YouTube (and Spotify, whose tracks play from
//! YouTube): tracks from elsewhere make room for its tracks, oldest first.
//!
//! A track stays while some player holds it: one whose queue repeats and still has it.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use entroximer_core::player::Kept;

use crate::api::protocol::Snowflake;

/// A player holding kept tracks: the bot's user ID and the guild's.
pub(crate) type Holder = (Snowflake, Snowflake);

/// Media the cache can keep: [`Kept`] in the engine, something simpler in tests.
pub(crate) trait Keepable {
    /// The bytes it takes on disk, if all of it is there.
    fn size(&self) -> Option<u64>;
    /// Keeps it for later, out of the budget of the tracks playing; `None` unless complete.
    fn park(&self) -> Option<u64>;
}

impl Keepable for Kept {
    fn size(&self) -> Option<u64> {
        Kept::size(self)
    }

    fn park(&self) -> Option<u64> {
        Kept::park(self)
    }
}

pub(crate) struct LoopCache<M = Kept> {
    /// Most bytes the kept tracks take on disk together; 0 keeps none.
    limit: u64,
    state: Mutex<State<M>>,
}

struct State<M> {
    /// By the tracks' `encoded` form.
    entries: HashMap<String, Entry<M>>,
    bytes: u64,
    next_seq: u64,
}

struct Entry<M> {
    media: M,
    bytes: u64,
    youtube: bool,
    /// When it was kept, for making room oldest first.
    seq: u64,
    holders: HashSet<Holder>,
}

impl<M: Keepable + Clone> LoopCache<M> {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            limit,
            state: Mutex::new(State {
                entries: HashMap::new(),
                bytes: 0,
                next_seq: 0,
            }),
        }
    }

    /// Keeps `media`, the track `key`'s, for `holder`, if all of it is on disk and there is
    /// room (see the module). Returns whether it is kept.
    pub(crate) fn offer(&self, key: &str, media: &M, youtube: bool, holder: Holder) -> bool {
        let mut state = self.state();
        if let Some(entry) = state.entries.get_mut(key) {
            entry.holders.insert(holder);
            return true;
        }
        let Some(bytes) = media.size() else {
            return false;
        };
        if bytes > self.limit || !state.make_room(bytes, youtube, self.limit) {
            return false;
        }
        let Some(bytes) = media.park() else {
            return false;
        };
        let seq = state.next_seq;
        state.next_seq += 1;
        state.bytes += bytes;
        state.entries.insert(
            key.to_owned(),
            Entry {
                media: media.clone(),
                bytes,
                youtube,
                seq,
                holders: HashSet::from([holder]),
            },
        );
        true
    }

    /// The kept media of the track `key`.
    pub(crate) fn get(&self, key: &str) -> Option<M> {
        self.state()
            .entries
            .get(key)
            .map(|entry| entry.media.clone())
    }

    /// `holder` no longer needs the track `key`; it goes once nobody does.
    pub(crate) fn release(&self, key: &str, holder: Holder) {
        let mut state = self.state();
        let unheld = state.entries.get_mut(key).is_some_and(|entry| {
            entry.holders.remove(&holder);
            entry.holders.is_empty()
        });
        if unheld {
            state.remove(key);
        }
    }

    /// The bytes the kept tracks take on disk.
    pub(crate) fn bytes(&self) -> u64 {
        self.state().bytes
    }

    fn state(&self) -> MutexGuard<'_, State<M>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<M> State<M> {
    /// Whether `bytes` more fit, after dropping tracks from elsewhere for a YouTube one. Drops
    /// none unless that makes enough room.
    fn make_room(&mut self, bytes: u64, youtube: bool, limit: u64) -> bool {
        if self.bytes + bytes <= limit {
            return true;
        }
        if !youtube {
            return false;
        }
        let mut others: Vec<(u64, String, u64)> = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.youtube)
            .map(|(key, entry)| (entry.seq, key.clone(), entry.bytes))
            .collect();
        let freeable: u64 = others.iter().map(|(.., bytes)| bytes).sum();
        if self.bytes - freeable + bytes > limit {
            return false;
        }
        others.sort_unstable();
        for (_, key, _) in others {
            if self.bytes + bytes <= limit {
                break;
            }
            self.remove(&key);
        }
        true
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes -= entry.bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    /// Media of a size, complete or not, that tells whether it was parked.
    #[derive(Clone)]
    struct Media {
        bytes: Option<u64>,
        parked: Rc<Cell<bool>>,
    }

    fn media(bytes: u64) -> Media {
        Media {
            bytes: Some(bytes),
            parked: Rc::default(),
        }
    }

    impl Keepable for Media {
        fn size(&self) -> Option<u64> {
            self.bytes
        }

        fn park(&self) -> Option<u64> {
            self.parked.set(true);
            self.bytes
        }
    }

    const A: Holder = (Snowflake(1), Snowflake(10));
    const B: Holder = (Snowflake(1), Snowflake(20));

    #[test]
    fn a_full_cache_keeps_what_it_has() {
        let cache = LoopCache::new(100);
        let first = media(60);
        assert!(cache.offer("one", &first, false, A));
        assert!(
            first.parked.get(),
            "kept media leaves the playing tracks' budget"
        );
        assert!(cache.offer("two", &media(40), false, A));
        let third = media(10);
        assert!(
            !cache.offer("three", &third, false, A),
            "nothing makes room by age"
        );
        assert!(!third.parked.get());
        assert_eq!(cache.bytes(), 100);
        assert!(!cache.offer("big", &media(101), true, A));
        let incomplete = Media {
            bytes: None,
            parked: Rc::default(),
        };
        assert!(!cache.offer("partial", &incomplete, false, A));
    }

    #[test]
    fn youtube_tracks_take_the_place_of_others() {
        let cache = LoopCache::new(100);
        assert!(cache.offer("yt1", &media(50), true, A));
        assert!(cache.offer("sc1", &media(30), false, A));
        assert!(cache.offer("sc2", &media(20), false, A));
        // the oldest other track makes room
        assert!(cache.offer("yt2", &media(30), true, A));
        assert!(cache.get("sc1").is_none());
        assert!(cache.get("sc2").is_some());
        assert_eq!(cache.bytes(), 100);
        // YouTube's own tracks are never dropped, and others are not dropped in vain
        assert!(!cache.offer("yt3", &media(30), true, A));
        assert!(cache.get("sc2").is_some());
        assert!(cache.get("yt1").is_some());
    }

    #[test]
    fn tracks_go_once_nobody_holds_them() {
        let cache = LoopCache::new(100);
        assert!(cache.offer("one", &media(10), true, A));
        assert!(cache.offer("one", &media(10), true, B), "held by both");
        assert_eq!(cache.bytes(), 10);
        cache.release("one", A);
        assert!(cache.get("one").is_some());
        cache.release("one", B);
        assert!(cache.get("one").is_none());
        assert_eq!(cache.bytes(), 0);
        cache.release("one", B);

        let off = LoopCache::new(0);
        assert!(!off.offer("one", &media(1), true, A), "0 keeps none");
    }
}
