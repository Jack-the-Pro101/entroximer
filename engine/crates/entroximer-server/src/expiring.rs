//! A small map whose entries expire, for caches that must stay bounded in time and size.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct Expiring<K, V> {
    entries: Mutex<HashMap<K, (V, Instant)>>,
    keep_for: Duration,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> Expiring<K, V> {
    /// Entries are kept for `keep_for`; beyond `capacity` the oldest make room.
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
            Some((value, at)) if at.elapsed() < self.keep_for => Some(value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub(crate) fn insert(&self, key: K, value: V) {
        let mut entries = self.entries();
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            entries.retain(|_, (_, at)| at.elapsed() < self.keep_for);
            if entries.len() >= self.capacity
                && let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, (value, Instant::now()));
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<K, (V, Instant)>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
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
    }

    #[test]
    fn entries_expire() {
        let cache = Expiring::new(Duration::ZERO, 8);
        cache.insert("a".to_owned(), ());
        assert_eq!(cache.get("a"), None, "keys are looked up by borrowed form");
    }
}
