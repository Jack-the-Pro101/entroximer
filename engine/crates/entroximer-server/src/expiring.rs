//! A small map whose entries expire, for caches that must stay bounded in time and size.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct Expiring<K, V> {
    /// With the time each expires.
    entries: Mutex<HashMap<K, (V, Instant)>>,
    keep_for: Duration,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> Expiring<K, V> {
    /// Entries are kept for `keep_for` unless inserted for longer or shorter; beyond `capacity`
    /// those that would expire first make room.
    pub(crate) fn new(keep_for: Duration, capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            keep_for,
            capacity,
        }
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut entries = self.entries();
        match entries.get(key) {
            Some((value, until)) if Instant::now() < *until => Some(value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub(crate) fn remove<Q>(&self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries().remove(key);
    }

    pub(crate) fn insert(&self, key: K, value: V) {
        self.insert_for(key, value, self.keep_for);
    }

    /// Inserts an entry kept for `keep_for` instead of the map's own time.
    pub(crate) fn insert_for(&self, key: K, value: V, keep_for: Duration) {
        let now = Instant::now();
        let mut entries = self.entries();
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            entries.retain(|_, (_, until)| now < *until);
            if entries.len() >= self.capacity
                && let Some(soonest) = entries
                    .iter()
                    .min_by_key(|(_, (_, until))| *until)
                    .map(|(key, _)| key.clone())
            {
                entries.remove(&soonest);
            }
        }
        entries.insert(key, (value, now + keep_for));
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<K, (V, Instant)>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_maps_make_room_by_age() {
        let cache = Expiring::new(Duration::from_secs(60), 3);
        for i in 0..5 {
            cache.insert(i, i * 10);
        }
        assert_eq!(cache.entries().len(), 3);
        assert_eq!(cache.get(&0), None, "the oldest made room");
        assert_eq!(cache.get(&4), Some(40));
        cache.insert(4, 41);
        assert_eq!(
            cache.entries().len(),
            3,
            "replacing an entry evicts nothing"
        );
        cache.remove(&4);
        assert_eq!(cache.get(&4), None);
    }

    #[test]
    fn entries_can_be_kept_for_their_own_time() {
        let cache = Expiring::new(Duration::from_secs(60), 2);
        cache.insert_for(1, "long", Duration::from_secs(3600));
        cache.insert_for(2, "gone", Duration::ZERO);
        assert_eq!(cache.get(&1), Some("long"));
        assert_eq!(cache.get(&2), None);
        cache.insert(3, "default");
        cache.insert(4, "full");
        assert_eq!(
            cache.get(&1),
            Some("long"),
            "those expiring first make room"
        );
    }

    #[test]
    fn entries_expire() {
        let cache = Expiring::new(Duration::ZERO, 8);
        cache.insert("a".to_owned(), ());
        assert_eq!(cache.get("a"), None, "keys are looked up by borrowed form");
    }
}
