//! A client's session: its players and the stream of messages to it.
//!
//! When the client's connection drops, its players pause where they are and the session waits
//! for its resume timeout. A client that reconnects with the session's ID in time finds its
//! players continuing from the same position, gets the events it could not receive (e.g. a load
//! that failed after the drop), and a fresh `playerUpdate` per player. Otherwise the players are
//! destroyed. Periodic events (`playerUpdate`, `stats`) are not kept: the fresh state replaces
//! them.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use super::Node;
use super::player::{ClipTurn, GuildPlayer};
use crate::api::protocol::{Event, ServerMessage, SessionSettings, Snowflake};

/// Events kept for a disconnected client; older ones are dropped and counted.
const BACKLOG_CAPACITY: usize = 1024;
/// The longest resume timeout a client may ask for.
const MAX_RESUME_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

pub struct Session {
    pub id: String,
    pub user_id: Snowflake,
    players: Mutex<HashMap<Snowflake, Arc<GuildPlayer>>>,
    link: Mutex<Link>,
    resume_timeout: Mutex<Duration>,
    connections: AtomicU64,
    /// For each guild's layer, what tells the next `clip.play` for it that the last one is in.
    clip_turns: Mutex<HashMap<(Snowflake, String), oneshot::Receiver<()>>>,
}

enum Link {
    Attached {
        connection: u64,
        out: UnboundedSender<ServerMessage>,
    },
    Detached {
        /// Identifies this stretch of being detached, see [`Session::detached_epoch`].
        epoch: u64,
        backlog: VecDeque<ServerMessage>,
        missed: u64,
    },
}

impl Session {
    pub(super) fn new(user_id: Snowflake, resume_timeout: Duration) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            user_id,
            players: Mutex::new(HashMap::new()),
            link: Mutex::new(Link::Detached {
                epoch: 0,
                backlog: VecDeque::new(),
                missed: 0,
            }),
            resume_timeout: Mutex::new(resume_timeout),
            connections: AtomicU64::new(1),
            clip_turns: Mutex::new(HashMap::new()),
        }
    }

    /// Makes `out` the session's connection: sends `ready`, any buffered events and each
    /// player's current state through it, and lets paused players continue. Returns the
    /// connection's number for [`detach`](Self::detach).
    pub fn attach(&self, out: UnboundedSender<ServerMessage>, resumed: bool) -> u64 {
        let connection = self.connections.fetch_add(1, Ordering::Relaxed);
        {
            let mut link = lock(&self.link);
            let (backlog, missed) = match std::mem::replace(
                &mut *link,
                Link::Attached {
                    connection,
                    out: out.clone(),
                },
            ) {
                Link::Detached {
                    backlog, missed, ..
                } => (backlog, missed),
                Link::Attached { .. } => (VecDeque::new(), 0),
            };
            let ready = ServerMessage::Ready {
                session_id: self.id.clone(),
                resumed,
                missed_events: missed,
            };
            // the receiving task outlives this call; if it is gone, the connection already closed
            for message in std::iter::once(ready).chain(backlog) {
                let _ = out.send(message);
            }
        }
        // players emit through the link, so only once it is released
        for player in self.players() {
            player.emit_update();
            player.set_client_present(true);
        }
        connection
    }

    /// Detaches `connection` if it is still the session's and pauses the players; returns the
    /// new detached epoch.
    pub(super) fn detach(&self, connection: u64) -> Option<u64> {
        let epoch = {
            let mut link = lock(&self.link);
            match &*link {
                Link::Attached { connection: c, .. } if *c == connection => {
                    let epoch = self.connections.fetch_add(1, Ordering::Relaxed);
                    *link = Link::Detached {
                        epoch,
                        backlog: VecDeque::new(),
                        missed: 0,
                    };
                    epoch
                }
                _ => return None,
            }
        };
        for player in self.players() {
            player.set_client_present(false);
        }
        Some(epoch)
    }

    /// The epoch of the current detached stretch, or `None` while a client is attached.
    pub(super) fn detached_epoch(&self) -> Option<u64> {
        match &*lock(&self.link) {
            Link::Detached { epoch, .. } => Some(*epoch),
            Link::Attached { .. } => None,
        }
    }

    pub fn emit(&self, event: Event) {
        // a resumed client gets fresh state instead
        let periodic = matches!(event, Event::PlayerUpdate { .. } | Event::Stats(_));
        let message = ServerMessage::Event(Box::new(event));
        let mut link = lock(&self.link);
        let message = match &*link {
            Link::Attached { out, .. } => match out.send(message) {
                Ok(()) => return,
                // the connection is closing; keep the event for a resume
                Err(unsent) => unsent.0,
            },
            Link::Detached { .. } => message,
        };
        if periodic {
            return;
        }
        if let Link::Detached {
            backlog, missed, ..
        } = &mut *link
        {
            if backlog.len() == BACKLOG_CAPACITY {
                backlog.pop_front();
                *missed += 1;
            }
            backlog.push_back(message);
        }
    }

    pub fn settings(&self) -> SessionSettings {
        SessionSettings {
            resume_timeout_secs: lock(&self.resume_timeout).as_secs(),
        }
    }

    /// Applies a client's settings; the result is what took effect.
    pub fn configure(&self, settings: SessionSettings) -> SessionSettings {
        *lock(&self.resume_timeout) =
            Duration::from_secs(settings.resume_timeout_secs).min(MAX_RESUME_TIMEOUT);
        self.settings()
    }

    pub(super) fn resume_timeout(&self) -> Duration {
        *lock(&self.resume_timeout)
    }

    /// The next turn of a `clip.play` for `layer`, to be taken as its request arrives: clips
    /// join their layer in the order they were sent, though they load at once.
    pub fn clip_turn(&self, guild_id: Snowflake, layer: &str) -> ClipTurn {
        let mut turns = lock(&self.clip_turns);
        // only turns still to finish matter, so that layers named once, or in refused
        // requests, are not kept for the session's lifetime
        turns.retain(|_, done| matches!(done.try_recv(), Err(TryRecvError::Empty)));
        let earlier = turns.remove(&(guild_id, layer.to_owned()));
        let (turn, done) = ClipTurn::after(earlier);
        turns.insert((guild_id, layer.to_owned()), done);
        turn
    }

    pub fn player(&self, guild_id: Snowflake) -> Option<Arc<GuildPlayer>> {
        lock(&self.players).get(&guild_id).cloned()
    }

    pub fn players(&self) -> Vec<Arc<GuildPlayer>> {
        lock(&self.players).values().cloned().collect()
    }

    pub fn player_or_create(
        self: &Arc<Self>,
        node: &Arc<Node>,
        guild_id: Snowflake,
    ) -> Arc<GuildPlayer> {
        lock(&self.players)
            .entry(guild_id)
            .or_insert_with(|| GuildPlayer::spawn(node, self, guild_id))
            .clone()
    }

    /// Stops and removes a player; returns whether there was one.
    pub async fn destroy_player(&self, guild_id: Snowflake) -> bool {
        let player = lock(&self.players).remove(&guild_id);
        match player {
            Some(player) => {
                player.destroy().await;
                true
            }
            None => false,
        }
    }

    pub(super) async fn destroy(&self) {
        let players: Vec<_> = lock(&self.players).drain().map(|(_, p)| p).collect();
        futures_util::future::join_all(players.iter().map(|player| player.destroy())).await;
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("players", &lock(&self.players).len())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;
    use crate::api::protocol::{Millis, Stats};
    use crate::youtube::SessionState;

    fn stats() -> Event {
        Event::Stats(Stats {
            players: 0,
            playing_players: 0,
            uptime: Millis(0),
            memory_bytes: 0,
            cache_memory_bytes: 0,
            cache_disk_bytes: 0,
            loop_cache_bytes: 0,
        })
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<ServerMessage>) -> Vec<serde_json::Value> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    }

    #[test]
    fn a_resumed_client_gets_kept_events_but_no_stale_periodic_ones() {
        let session = Session::new(Snowflake(1), Duration::from_secs(60));
        let (out, mut rx) = mpsc::unbounded_channel();
        let first = session.attach(out, false);
        assert_eq!(drain(&mut rx)[0]["type"], "ready");

        session.detach(first).unwrap();
        session.emit(stats());
        session.emit(Event::YoutubeSession {
            state: SessionState::Expired,
        });
        assert!(session.detached_epoch().is_some());

        let (out, mut rx) = mpsc::unbounded_channel();
        session.attach(out, true);
        let received = drain(&mut rx);
        assert_eq!(received.len(), 2, "{received:?}");
        assert_eq!(received[0]["resumed"], true);
        assert_eq!(received[0]["missedEvents"], 0);
        assert_eq!(
            received[1],
            json!({ "type": "event", "event": "youtubeSession", "state": "expired" })
        );
        assert!(session.detached_epoch().is_none());
    }

    #[test]
    fn resume_timeouts_are_capped_at_a_day() {
        let session = Session::new(Snowflake(1), Duration::from_secs(60));
        let applied = session.configure(SessionSettings {
            resume_timeout_secs: u64::MAX,
        });
        assert_eq!(applied.resume_timeout_secs, 24 * 3600);
    }

    #[test]
    fn only_the_current_connection_detaches() {
        let session = Session::new(Snowflake(1), Duration::from_secs(60));
        let (out, _rx) = mpsc::unbounded_channel();
        let old = session.attach(out, false);
        let (out, _rx2) = mpsc::unbounded_channel();
        let current = session.attach(out, true);
        assert!(
            session.detach(old).is_none(),
            "a replaced connection closing changes nothing"
        );
        assert!(session.detach(current).is_some());
    }
}
