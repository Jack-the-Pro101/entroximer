//! A player's queue: the tracks that play after the current one.
//!
//! This is plain data. The player decides when to prepare the next item and starts it, loads
//! the queries of loading items, and sends items that finish back to the end when repeating the
//! queue (see `player`). Every change here returns a [`QueueChange`] for clients to mirror,
//! numbered by [`Queue::version`] so that a client noticing a gap can fetch the whole queue again.

use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize, Serializer};
use ts_rs::TS;

use crate::api::protocol::{Millis, Track};
use crate::sources::PlaylistInfo;

/// A track in the queue, or playing. Its id stays the same while it moves around.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct QueueItem {
    pub id: String,
    pub track: Track,
    /// Whether the engine is still loading the item's query. Its `track` then only stands in
    /// for what it finds: the title and `uri` are the link or search as given, the `sourceName`
    /// is `query`. A `loaded` change replaces the item with the tracks it found.
    #[serde(rename = "loading", serialize_with = "is_pending")]
    #[ts(rename = "loading", type = "boolean")]
    pub(crate) pending: Option<Box<Pending>>,
}

/// What a loading item stands for: a query, from `start_index` if it is a playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub query: String,
    pub start_index: Option<usize>,
    /// Loads at once, as `tracks.load` would, rather than in queue order: a client is waiting
    /// for it, e.g. a bot's `/play`.
    pub now: bool,
}

impl QueueItem {
    pub fn new(track: Track) -> Self {
        Self {
            id: new_id(),
            track,
            pending: None,
        }
    }

    /// An item for something not loaded yet, shown as `stand_in` meanwhile.
    pub fn loading(stand_in: Track, pending: Pending) -> Self {
        Self {
            id: new_id(),
            track: stand_in,
            pending: Some(Box::new(pending)),
        }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_deref()
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn is_pending<S: Serializer>(pending: &Option<Box<Pending>>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bool(pending.is_some())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum Repeat {
    #[default]
    Off,
    /// The current track plays again and again.
    Track,
    /// Tracks that finish go to the end of the queue.
    Queue,
}

/// One change to a queue, as sent to clients.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum QueueChange {
    #[serde(rename_all = "camelCase")]
    Added {
        index: u32,
        items: Vec<QueueItem>,
    },
    Removed {
        ids: Vec<String>,
    },
    Moved {
        id: String,
        index: u32,
    },
    Cleared,
    /// The queue's new order.
    Shuffled {
        order: Vec<String>,
    },
    /// An item's track got fuller metadata, e.g. once resolved for playback, or new times
    /// (`queue.update`).
    Updated {
        item: Box<QueueItem>,
    },
    /// The loading item `id` loaded: it is replaced, in place, by `items`, the first of which
    /// keeps its id. `playlist` describes the playlist the items came from.
    Loaded {
        id: String,
        items: Vec<QueueItem>,
        playlist: Option<PlaylistInfo>,
    },
    /// The first item is starting and leaves the queue; its `trackStart` follows.
    Advanced {
        id: String,
    },
    Repeat {
        repeat: Repeat,
    },
}

#[derive(Debug, Default)]
pub struct Queue {
    items: VecDeque<QueueItem>,
    repeat: Repeat,
    /// How long the end of each track overlaps the start of the next.
    crossfade: Millis,
    version: u64,
}

impl Queue {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn crossfade(&self) -> Millis {
        self.crossfade
    }

    /// Only clients change it, so it is not a change clients need to hear about.
    pub fn set_crossfade(&mut self, crossfade: Millis) {
        self.crossfade = crossfade;
    }

    pub fn head(&self) -> Option<&QueueItem> {
        self.items.front()
    }

    pub fn page(&self, offset: usize, limit: usize) -> Vec<QueueItem> {
        self.items
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &QueueItem> {
        self.items.iter()
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.id == id)
    }

    pub fn get(&self, id: &str) -> Option<&QueueItem> {
        self.items.iter().find(|item| item.id == id)
    }

    /// The item to play after `current` ends on its own: `current` again when repeating it (or
    /// when repeating a queue that holds nothing else), else the first item.
    pub fn upcoming<'a>(&'a self, current: Option<&'a QueueItem>) -> Option<&'a QueueItem> {
        match (self.repeat, current) {
            (Repeat::Track, Some(current)) => Some(current),
            (Repeat::Queue, Some(current)) if self.items.is_empty() => Some(current),
            _ => self.items.front(),
        }
    }

    /// Inserts items at `index` (the end if `None` or past it).
    pub fn add(&mut self, items: Vec<QueueItem>, index: Option<usize>) -> Option<QueueChange> {
        if items.is_empty() {
            return None;
        }
        let index = index.unwrap_or(self.items.len()).min(self.items.len());
        let mut after = self.items.split_off(index);
        self.items.extend(items.iter().cloned());
        self.items.append(&mut after);
        Some(self.changed(QueueChange::Added {
            index: index_u32(index),
            items,
        }))
    }

    /// Removes the items with these ids; ids not in the queue are ignored.
    pub fn remove(&mut self, ids: &[String]) -> Option<QueueChange> {
        let ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let removed: Vec<String> = self
            .items
            .iter()
            .filter(|item| ids.contains(item.id.as_str()))
            .map(|item| item.id.clone())
            .collect();
        if removed.is_empty() {
            return None;
        }
        self.items.retain(|item| !ids.contains(item.id.as_str()));
        Some(self.changed(QueueChange::Removed { ids: removed }))
    }

    /// Moves an item to `index`, counted after its removal (the end if past it).
    pub fn move_item(&mut self, id: &str, index: usize) -> Option<QueueChange> {
        let from = self.position(id)?;
        let item = self.items.remove(from)?;
        let index = index.min(self.items.len());
        self.items.insert(index, item);
        Some(self.changed(QueueChange::Moved {
            id: id.to_owned(),
            index: index_u32(index),
        }))
    }

    pub fn clear(&mut self) -> Option<QueueChange> {
        if self.items.is_empty() {
            return None;
        }
        self.items.clear();
        Some(self.changed(QueueChange::Cleared))
    }

    pub fn shuffle(&mut self, rng: &mut fastrand::Rng) -> Option<QueueChange> {
        if self.items.len() < 2 {
            return None;
        }
        rng.shuffle(self.items.make_contiguous());
        let order = self.items.iter().map(|item| item.id.clone()).collect();
        Some(self.changed(QueueChange::Shuffled { order }))
    }

    pub fn set_repeat(&mut self, repeat: Repeat) -> Option<QueueChange> {
        if self.repeat == repeat {
            return None;
        }
        self.repeat = repeat;
        Some(self.changed(QueueChange::Repeat { repeat }))
    }

    /// Replaces an item's track, keeping its id and place.
    pub fn update(&mut self, id: &str, track: Track) -> Option<QueueChange> {
        let position = self.position(id)?;
        let item = &mut self.items[position];
        if item.track == track {
            return None;
        }
        item.track = track;
        let item = item.clone();
        Some(self.changed(QueueChange::Updated {
            item: Box::new(item),
        }))
    }

    /// Replaces the loading item `id`, in place, by `items`. The first of them should have its
    /// id.
    pub fn replace(
        &mut self,
        id: &str,
        items: Vec<QueueItem>,
        playlist: Option<PlaylistInfo>,
    ) -> Option<QueueChange> {
        let position = self.position(id)?;
        let mut after = self.items.split_off(position);
        after.pop_front();
        self.items.extend(items.iter().cloned());
        self.items.append(&mut after);
        Some(self.changed(QueueChange::Loaded {
            id: id.to_owned(),
            items,
            playlist,
        }))
    }

    /// `started` began playing: if it is the first item, it leaves the queue.
    pub fn advance(&mut self, started: &QueueItem) -> Option<QueueChange> {
        if self.head()?.id != started.id {
            return None;
        }
        self.items.pop_front();
        Some(self.changed(QueueChange::Advanced {
            id: started.id.clone(),
        }))
    }

    fn changed(&mut self, change: QueueChange) -> QueueChange {
        self.version += 1;
        change
    }
}

fn index_u32(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::TrackInfo;

    fn item(name: &str) -> QueueItem {
        QueueItem {
            id: name.to_owned(),
            pending: None,
            track: Track {
                encoded: name.to_owned(),
                info: TrackInfo {
                    identifier: name.to_owned(),
                    title: name.to_owned(),
                    author: None,
                    author_url: None,
                    album: None,
                    length: None,
                    is_stream: false,
                    is_seekable: true,
                    uri: name.to_owned(),
                    artwork_url: None,
                    isrc: None,
                    source_name: "test".to_owned(),
                },
                user_data: None,
                start_time: None,
                end_time: None,
            },
        }
    }

    fn ids(queue: &Queue) -> Vec<String> {
        queue
            .page(0, usize::MAX)
            .into_iter()
            .map(|i| i.id)
            .collect()
    }

    #[test]
    fn edits_report_changes_and_bump_the_version() {
        let mut queue = Queue::default();
        queue.add(vec![item("a"), item("b")], None).unwrap();
        let change = queue.add(vec![item("c")], Some(1)).unwrap();
        assert_eq!(ids(&queue), ["a", "c", "b"]);
        assert!(matches!(change, QueueChange::Added { index: 1, .. }));

        assert_eq!(
            queue.move_item("a", 99),
            Some(QueueChange::Moved {
                id: "a".into(),
                index: 2
            })
        );
        assert_eq!(ids(&queue), ["c", "b", "a"]);
        assert_eq!(
            queue.remove(&["b".into(), "zz".into()]),
            Some(QueueChange::Removed {
                ids: vec!["b".into()]
            })
        );
        assert_eq!(queue.remove(&["zz".into()]), None, "no-ops change nothing");
        assert_eq!(queue.version(), 4);
        assert_eq!(queue.clear(), Some(QueueChange::Cleared));
        assert!(queue.is_empty());
    }

    #[test]
    fn shuffling_keeps_every_item() {
        let mut queue = Queue::default();
        queue
            .add((0..20).map(|i| item(&i.to_string())).collect(), None)
            .unwrap();
        let Some(QueueChange::Shuffled { order }) = queue.shuffle(&mut fastrand::Rng::with_seed(7))
        else {
            panic!("expected a shuffle");
        };
        assert_eq!(order, ids(&queue));
        let mut sorted = order.clone();
        sorted.sort_by_key(|id| id.parse::<u32>().unwrap());
        assert_eq!(sorted, (0..20).map(|i| i.to_string()).collect::<Vec<_>>());
        assert_ne!(order, sorted, "seed 7 moves something");
    }

    #[test]
    fn the_upcoming_item_follows_the_repeat_mode() {
        let mut queue = Queue::default();
        let a = item("a");
        assert_eq!(queue.upcoming(Some(&a)), None, "nothing follows");
        queue.set_repeat(Repeat::Queue);
        assert_eq!(
            queue.upcoming(Some(&a)),
            Some(&a),
            "a repeated queue of one track plays it again"
        );

        queue.add(vec![item("b"), item("c")], None);
        queue.set_repeat(Repeat::Off);
        assert_eq!(queue.upcoming(Some(&a)).unwrap().id, "b");
        assert_eq!(queue.upcoming(None).unwrap().id, "b");
        queue.set_repeat(Repeat::Queue);
        assert_eq!(queue.upcoming(Some(&a)).unwrap().id, "b");
        queue.set_repeat(Repeat::Track);
        assert_eq!(queue.upcoming(Some(&a)), Some(&a));
        assert_eq!(
            queue.upcoming(None).unwrap().id,
            "b",
            "with nothing to repeat, the queue goes on"
        );
    }

    #[test]
    fn only_the_first_item_advances() {
        let mut queue = Queue::default();
        queue.add(vec![item("b"), item("c")], None);
        assert_eq!(
            queue.advance(&item("c")),
            None,
            "c did not come from the front"
        );
        assert_eq!(
            queue.advance(&item("b")),
            Some(QueueChange::Advanced { id: "b".into() })
        );
        assert_eq!(ids(&queue), ["c"]);
        assert_eq!(queue.version(), 2);
    }

    #[test]
    fn loaded_items_take_the_place_of_the_loading_one() {
        let mut queue = Queue::default();
        let loading = QueueItem {
            id: "q".into(),
            pending: Some(Box::new(Pending {
                query: "ytsearch:x".into(),
                start_index: None,
                now: false,
            })),
            ..item("q")
        };
        queue.add(vec![item("a"), loading, item("b")], None);
        let change = queue
            .replace("q", vec![item("q"), item("q2")], None)
            .unwrap();
        assert!(
            matches!(change, QueueChange::Loaded { ref id, ref items, .. } if id == "q" && items.len() == 2)
        );
        assert_eq!(ids(&queue), ["a", "q", "q2", "b"]);
        assert!(queue.iter().all(|item| !item.is_loading()));
        assert_eq!(queue.version(), 2);

        queue.replace("q2", Vec::new(), None).unwrap();
        assert_eq!(ids(&queue), ["a", "q", "b"]);
        assert_eq!(queue.replace("gone", Vec::new(), None), None);
    }

    #[test]
    fn items_tell_clients_whether_they_are_loading() {
        let mut loading = item("q");
        loading.pending = Some(Box::new(Pending {
            query: "x".into(),
            start_index: Some(3),
            now: false,
        }));
        let json = serde_json::to_value(&loading).unwrap();
        assert_eq!(json["loading"], true);
        assert_eq!(serde_json::to_value(item("a")).unwrap()["loading"], false);
    }

    #[test]
    fn updates_keep_the_item_in_place() {
        let mut queue = Queue::default();
        queue.add(vec![item("a"), item("b")], None);
        let mut fuller = item("b").track;
        fuller.info.author = Some("Someone".into());
        let change = queue.update("b", fuller.clone()).unwrap();
        assert!(matches!(change, QueueChange::Updated { ref item } if item.track == fuller));
        assert_eq!(
            queue.update("b", fuller),
            None,
            "unchanged tracks are not reported"
        );
        assert_eq!(ids(&queue), ["a", "b"]);
    }
}
