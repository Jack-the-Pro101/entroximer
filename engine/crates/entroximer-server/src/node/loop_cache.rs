//! Tracks of repeated queues kept on disk once they have played, so that the queue's next round
//! plays them from there instead of loading them again: no yt-dlp run and no download, which
//! matters most for YouTube, which flags addresses that ask it for too much.
//!
//! Only tracks whose media is on disk, as far as they play it (all of it, or from their start
//! time to their end time), are kept, together at most `ENTROXIMER_LOOP_CACHE_MB`
//! (shared by every player, apart from what the tracks playing may take). When that is full, the
//! cache keeps what it has: a repeated queue plays its tracks in a cycle, so making room by age
//! would drop each track just before it comes round again, and a queue too long for the cache
//! would gain nothing at all. The one exception is YouTube (and Spotify, whose tracks play from
//! YouTube): tracks from elsewhere make room for its tracks, oldest first.
//!
//! A track is kept once, with the part of it that downloaded: an item playing a part of it that
//! is there plays from there, whatever its times, and one whose part is missing (its times
//! changed) loads again and takes its place. A track stays while some player holds it: one whose
//! queue repeats and still has it.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use entroximer_core::player::Kept;

use crate::api::protocol::{Snowflake, Times};

/// A player holding kept tracks: the bot's user ID and the guild's.
pub(crate) type Holder = (Snowflake, Snowflake);

/// Media the cache can keep: [`Kept`] in the engine, something simpler in tests.
pub(crate) trait Keepable {
    /// The bytes it takes on disk, if all of the part `times` plays is there.
    fn size(&self, times: Times) -> Option<u64>;
    /// Keeps the part `times` plays for later, out of the budget of the tracks playing; `None`
    /// unless all of it is on disk.
    fn park(&self, times: Times) -> Option<u64>;
}

impl Keepable for Kept {
    fn size(&self, times: Times) -> Option<u64> {
        Kept::size(self, times.start_ticks().unwrap_or(0), times.end_ticks())
    }

    fn park(&self, times: Times) -> Option<u64> {
        Kept::park(self, times.start_ticks().unwrap_or(0), times.end_ticks())
    }
}

pub(crate) struct LoopCache<M = Kept> {
    /// Most bytes the kept tracks take on disk together; 0 keeps none.
    limit: u64,
    state: Mutex<State<M>>,
}

struct State<M> {
    /// By what the tracks play: see `player::rounds::kept_key`.
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

    /// Keeps `media`, the track `key`'s, for `holder`, if the part `times` of it that plays is
    /// on disk and there is room (see the module). Returns whether it is kept, by now or before.
    pub(crate) fn offer(
        &self,
        key: &str,
        media: &M,
        times: Times,
        youtube: bool,
        holder: Holder,
    ) -> bool {
        let mut state = self.state();
        if let Some(entry) = state.entries.get_mut(key)
            && entry.media.size(times).is_some()
        {
            entry.holders.insert(holder);
            return true;
        }
        // new, or in place of another part of the track
        let Some(bytes) = media.size(times) else {
            return false;
        };
        let replaced = state.remove(key);
        let parked = (bytes <= self.limit && state.make_room(bytes, youtube, self.limit))
            .then(|| media.park(times))
            .flatten();
        let Some(bytes) = parked else {
            // the part kept before stays
            if let Some(entry) = replaced {
                state.add(key, entry);
            }
            return false;
        };
        let mut holders = replaced.map(|entry| entry.holders).unwrap_or_default();
        holders.insert(holder);
        let seq = state.next_seq;
        state.next_seq += 1;
        state.add(
            key,
            Entry {
                media: media.clone(),
                bytes,
                youtube,
                seq,
                holders,
            },
        );
        true
    }

    /// The kept media of the track `key`, if it holds the part `times` plays.
    pub(crate) fn get(&self, key: &str, times: Times) -> Option<M> {
        self.state()
            .entries
            .get(key)
            .map(|entry| &entry.media)
            .filter(|media| media.size(times).is_some())
            .cloned()
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

    /// Drops the track `key`, whoever holds it, e.g. after it failed to play from here.
    pub(crate) fn discard(&self, key: &str) {
        self.state().remove(key);
    }

    /// The bytes the kept tracks take on disk.
    pub(crate) fn bytes(&self) -> u64 {
        self.state().bytes
    }

    /// The most bytes the kept tracks may take on disk.
    pub(crate) fn limit(&self) -> u64 {
        self.limit
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

    fn add(&mut self, key: &str, entry: Entry<M>) {
        self.bytes += entry.bytes;
        self.entries.insert(key.to_owned(), entry);
    }

    fn remove(&mut self, key: &str) -> Option<Entry<M>> {
        let entry = self.entries.remove(key)?;
        self.bytes -= entry.bytes;
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::api::protocol::Millis;

    /// Media of a size, downloaded or not, from a start (ms) to its end, that tells whether it
    /// was parked.
    #[derive(Clone)]
    struct Media {
        bytes: Option<u64>,
        from: u64,
        parked: Rc<Cell<bool>>,
    }

    fn media(bytes: u64) -> Media {
        media_from(bytes, 0)
    }

    fn media_from(bytes: u64, from: u64) -> Media {
        Media {
            bytes: Some(bytes),
            from,
            parked: Rc::default(),
        }
    }

    fn from(ms: u64) -> Times {
        Times {
            start: Some(Millis(ms)),
            end: None,
        }
    }

    impl Keepable for Media {
        fn size(&self, times: Times) -> Option<u64> {
            self.bytes
                .filter(|_| times.start.map_or(0, |start| start.0) >= self.from)
        }

        fn park(&self, times: Times) -> Option<u64> {
            let bytes = self.size(times)?;
            self.parked.set(true);
            Some(bytes)
        }
    }

    const ALL: Times = Times {
        start: None,
        end: None,
    };
    const A: Holder = (Snowflake(1), Snowflake(10));
    const B: Holder = (Snowflake(1), Snowflake(20));

    #[test]
    fn a_full_cache_keeps_what_it_has() {
        let cache = LoopCache::new(100);
        let first = media(60);
        assert!(cache.offer("one", &first, ALL, false, A));
        assert!(
            first.parked.get(),
            "kept media leaves the playing tracks' budget"
        );
        assert!(cache.offer("two", &media(40), ALL, false, A));
        let third = media(10);
        assert!(
            !cache.offer("three", &third, ALL, false, A),
            "nothing makes room by age"
        );
        assert!(!third.parked.get());
        assert_eq!(cache.bytes(), 100);
        assert!(!cache.offer("big", &media(101), ALL, true, A));
        let incomplete = Media {
            bytes: None,
            ..media(0)
        };
        assert!(!cache.offer("partial", &incomplete, ALL, false, A));
    }

    #[test]
    fn youtube_tracks_take_the_place_of_others() {
        let cache = LoopCache::new(100);
        assert!(cache.offer("yt1", &media(50), ALL, true, A));
        assert!(cache.offer("sc1", &media(30), ALL, false, A));
        assert!(cache.offer("sc2", &media(20), ALL, false, A));
        // the oldest other track makes room
        assert!(cache.offer("yt2", &media(30), ALL, true, A));
        assert!(cache.get("sc1", ALL).is_none());
        assert!(cache.get("sc2", ALL).is_some());
        assert_eq!(cache.bytes(), 100);
        // YouTube's own tracks are never dropped, and others are not dropped in vain
        assert!(!cache.offer("yt3", &media(30), ALL, true, A));
        assert!(cache.get("sc2", ALL).is_some());
        assert!(cache.get("yt1", ALL).is_some());
    }

    #[test]
    fn tracks_go_once_nobody_holds_them() {
        let cache = LoopCache::new(100);
        assert!(cache.offer("one", &media(10), ALL, true, A));
        assert!(cache.offer("one", &media(10), ALL, true, B), "held by both");
        assert_eq!(cache.bytes(), 10);
        cache.release("one", A);
        assert!(cache.get("one", ALL).is_some());
        cache.release("one", B);
        assert!(cache.get("one", ALL).is_none());
        assert_eq!(cache.bytes(), 0);
        cache.release("one", B);

        assert!(cache.offer("two", &media(10), ALL, true, A));
        cache.discard("two");
        assert!(cache.get("two", ALL).is_none(), "dropped although held");
        assert_eq!(cache.bytes(), 0);

        let off = LoopCache::new(0);
        assert!(!off.offer("one", &media(1), ALL, true, A), "0 keeps none");
    }

    #[test]
    fn a_track_plays_any_part_it_kept_and_a_missing_part_takes_its_place() {
        let cache = LoopCache::new(100);
        assert!(cache.offer("one", &media_from(30, 10_000), from(10_000), true, A));
        assert!(
            cache.get("one", from(20_000)).is_some(),
            "a later start is there"
        );
        assert!(cache.get("one", ALL).is_none(), "the beginning is not");
        assert!(
            cache.offer("one", &media_from(30, 10_000), from(20_000), true, B),
            "held by both"
        );

        // its times changed: what it played now takes the place of what was kept
        assert!(!cache.offer("one", &media_from(30, 5_000), ALL, true, A));
        assert!(
            cache.get("one", from(10_000)).is_some(),
            "what it kept stays"
        );
        assert!(cache.offer("one", &media(50), ALL, true, A));
        assert!(cache.get("one", ALL).is_some());
        assert_eq!(cache.bytes(), 50);
        cache.release("one", A);
        assert!(cache.get("one", ALL).is_some(), "still held by B");
        cache.release("one", B);
        assert_eq!(cache.bytes(), 0);

        // nor when there is no room for it
        assert!(cache.offer("two", &media_from(60, 10_000), from(10_000), true, A));
        assert!(cache.offer("three", &media(40), ALL, true, A));
        assert!(!cache.offer("two", &media(70), ALL, true, A));
        assert!(cache.get("two", from(10_000)).is_some());
        assert_eq!(cache.bytes(), 100);
    }
}
