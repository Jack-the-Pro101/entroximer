//! The engine's runtime state: client sessions, their players, and recently loaded tracks.

mod backoff;
mod loop_cache;
mod output;
pub mod player;
pub mod queue;
pub mod session;
pub mod tracks;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use entroximer_core::dsp::{NodeRegistry, PresetRegistry};
use futures_util::future::join_all;
use tokio::sync::{OnceCell, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub use session::Session;

use crate::api::protocol::{
    ApiError, ErrorCode, Event, Millis, NodeInfo, PROTOCOL_VERSION, Snowflake, Stats, Track,
    TrackSelector, UploadsInfo, as_index,
};
use crate::config::{MediaContext, Secret};
use crate::managed_ytdlp::YtDlpBinary;
use crate::sources::QUERY_SOURCE;
use backoff::Backoff;
use loop_cache::LoopCache;
use queue::{Pending, QueueItem};
use tracks::{Loaded, TrackCache, decode, stand_in, with_user_data};

#[derive(Clone, Debug)]
pub struct NodeSettings {
    /// What clients must send as `Authorization`; `None` lets every client in.
    pub password: Option<Secret>,
    pub player_update_interval: Duration,
    /// After waiting this long for media, a track is reported stuck.
    pub stuck_after: Duration,
    /// How long a disconnected session's players stay paused before they are destroyed, unless
    /// the client changes it.
    pub resume_timeout: Duration,
    /// How long before the current track ends a player prepares the next item of its queue.
    pub preload_lead: Duration,
}

pub struct Node {
    pub ctx: Arc<MediaContext>,
    pub settings: NodeSettings,
    /// Effect node types.
    pub registry: NodeRegistry,
    pub presets: PresetRegistry,
    pub tracks: TrackCache,
    /// Clips loading at once, engine-wide: a burst of them must not start a burst of
    /// downloads.
    pub clip_loads: Semaphore,
    /// Whether queues' background loads wait out rate limiting.
    pub(crate) load_backoff: Backoff,
    /// Tracks of repeated queues kept for their next round.
    pub(crate) loop_cache: LoopCache,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    system_ytdlp_version: OnceCell<Option<String>>,
    started: Instant,
    /// Cancelled when the engine shuts down; client connections then close.
    closing: CancellationToken,
    /// The client connections, so that shutting down can wait for them to close.
    connections: TaskTracker,
}

impl Node {
    const STATS_INTERVAL: Duration = Duration::from_secs(60);
    /// How long shutting down waits for the last messages to reach clients.
    const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
    const CLIP_LOADS: usize = 8;

    pub fn new(ctx: Arc<MediaContext>, settings: NodeSettings) -> Arc<Self> {
        let loop_cache = LoopCache::new(ctx.config.loop_cache_bytes());
        let node = Arc::new(Self {
            ctx,
            settings,
            registry: NodeRegistry::with_builtins(),
            presets: PresetRegistry::with_builtins(),
            tracks: TrackCache::default(),
            clip_loads: Semaphore::new(Self::CLIP_LOADS),
            load_backoff: Backoff::default(),
            loop_cache,
            sessions: Mutex::new(HashMap::new()),
            system_ytdlp_version: OnceCell::new(),
            started: Instant::now(),
            closing: CancellationToken::new(),
            connections: TaskTracker::new(),
        });
        tokio::spawn(periodic_events(Arc::downgrade(&node)));
        tokio::spawn(forward_youtube_session(
            Arc::downgrade(&node),
            node.ctx.youtube.subscribe(),
        ));
        node
    }

    /// Resumes the session `resume` names if it exists, belongs to the same bot and has no
    /// connection; otherwise starts a new one. Returns whether it resumed.
    pub fn open_session(&self, user_id: Snowflake, resume: Option<&str>) -> (Arc<Session>, bool) {
        let mut sessions = self.sessions();
        if let Some(session) = resume
            .and_then(|id| sessions.get(id))
            .filter(|s| s.user_id == user_id && s.detached_epoch().is_some())
        {
            return (Arc::clone(session), true);
        }
        let session = Arc::new(Session::new(user_id, self.settings.resume_timeout));
        sessions.insert(session.id.clone(), Arc::clone(&session));
        (session, false)
    }

    /// The client's connection closed: pause its players and keep the session for its resume
    /// timeout.
    pub fn connection_closed(self: &Arc<Self>, session: &Arc<Session>, connection: u64) {
        let Some(epoch) = session.detach(connection) else {
            return;
        };
        let timeout = session.resume_timeout();
        let (node, session) = (Arc::clone(self), Arc::clone(session));
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if session.detached_epoch() == Some(epoch) {
                tracing::info!(session = %session.id, "session was not resumed; destroying its players");
                node.sessions().remove(&session.id);
                session.destroy().await;
            }
        });
    }

    /// Whether the engine is shutting down, and takes no new connections.
    pub fn is_closing(&self) -> bool {
        self.closing.is_cancelled()
    }

    /// A handle that is cancelled once the engine shuts down.
    pub(crate) fn closing_signal(&self) -> CancellationToken {
        self.closing.clone()
    }

    /// Resolves once the engine shuts down.
    pub(crate) fn closed(&self) -> impl Future<Output = ()> + '_ {
        self.closing.cancelled()
    }

    /// Runs a client connection, which shutting down waits for.
    pub(crate) fn track_connection<F: Future<Output = ()>>(
        &self,
        connection: F,
    ) -> impl Future<Output = ()> + use<F> {
        self.connections.track_future(connection)
    }

    /// Ends every session for a shutdown: the players stop and leave voice (their clients get
    /// `trackEnd` with reason `cleanup`), then every connection closes as "going away", and this
    /// waits a few seconds for that to reach the clients.
    pub async fn shutdown(&self) {
        let sessions: Vec<_> = self
            .sessions()
            .drain()
            .map(|(_, session)| session)
            .collect();
        tracing::info!(sessions = sessions.len(), "ending client sessions");
        join_all(sessions.iter().map(|session| session.destroy())).await;
        self.closing.cancel();
        self.connections.close();
        if tokio::time::timeout(Self::SHUTDOWN_GRACE, self.connections.wait())
            .await
            .is_err()
        {
            tracing::warn!("some client connections did not close in time");
        }
    }

    pub async fn load(&self, selector: &TrackSelector) -> Result<Loaded, ApiError> {
        let loaded = match selector {
            TrackSelector::Encoded { encoded, .. } => self.tracks.get(&self.ctx, encoded).await?,
            TrackSelector::Query {
                query, start_index, ..
            } => {
                self.tracks
                    .resolve_query(&self.ctx, query, as_index(*start_index))
                    .await?
            }
        };
        Ok(with_user_data(loaded, selector.user_data()))
    }

    /// The item a selector adds to a queue: an encoded track ready to queue (it is resolved
    /// when it comes up), a query (or a stand-in for one) as a loading item, which the player
    /// loads in the background.
    pub fn queue_item(&self, selector: &TrackSelector) -> Result<QueueItem, ApiError> {
        Ok(match selector {
            TrackSelector::Encoded { encoded, user_data } => {
                let found = decode(encoded)?;
                let track = Track {
                    encoded: encoded.clone(),
                    info: found.info,
                    user_data: user_data.clone(),
                };
                if found.reference.source == QUERY_SOURCE {
                    let query = found.reference.uri;
                    let pending = Pending {
                        query,
                        start_index: None,
                        now: false,
                    };
                    QueueItem::loading(track, pending)
                } else {
                    QueueItem::new(track)
                }
            }
            TrackSelector::Query {
                query,
                start_index,
                user_data,
            } => {
                let query = query.trim();
                if query.is_empty() {
                    return Err(ApiError::new(ErrorCode::BadRequest, "a query is empty"));
                }
                QueueItem::loading(
                    stand_in(query, user_data.as_ref()),
                    Pending {
                        query: query.to_owned(),
                        start_index: as_index(*start_index),
                        now: false,
                    },
                )
            }
        })
    }

    pub fn stats(&self) -> Stats {
        let players: Vec<_> = self.sessions().values().flat_map(|s| s.players()).collect();
        let (cache_memory_bytes, cache_disk_bytes) = players
            .iter()
            .map(|p| p.cache_stats())
            .fold((0, 0), |(m, d), s| {
                (m + s.memory_bytes as u64, d + s.disk_bytes)
            });
        Stats {
            players: u32::try_from(players.len()).unwrap_or(u32::MAX),
            playing_players: u32::try_from(players.iter().filter(|p| p.is_playing()).count())
                .unwrap_or(u32::MAX),
            uptime: Millis::from(self.started.elapsed()),
            memory_bytes: resident_memory().unwrap_or(0),
            cache_memory_bytes,
            cache_disk_bytes,
            cache_memory_limit_bytes: self.ctx.config.memory_total_bytes(),
            cache_disk_limit_bytes: self.ctx.config.disk_total_bytes(),
            loop_cache_bytes: self.loop_cache.bytes(),
            loop_cache_limit_bytes: self.loop_cache.limit(),
        }
    }

    pub async fn info(&self) -> NodeInfo {
        let ytdlp = match &self.ctx.ytdlp {
            YtDlpBinary::Managed(managed) => managed.version().await,
            // asked once: replacing it means restarting the engine
            YtDlpBinary::System(_) => self
                .system_ytdlp_version
                .get_or_init(|| async {
                    self.ctx
                        .ytdlp
                        .command()
                        .arg("--version")
                        .output()
                        .await
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                })
                .await
                .clone(),
        };
        NodeInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: PROTOCOL_VERSION,
            ytdlp,
            youtube_session: self
                .ctx
                .youtube
                .store
                .status()
                .map(|s| s.state)
                .unwrap_or_default(),
            providers: self.ctx.sources.descriptors(),
            nodes: self.registry.descriptors().cloned().collect(),
            presets: self.presets.descriptors().cloned().collect(),
            uploads: self.ctx.uploads.as_ref().map(|store| {
                let limits = store.limits();
                UploadsInfo {
                    max_bytes: limits.max_bytes,
                    keep: Millis::from(limits.keep),
                }
            }),
        }
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn broadcast(&self, event: &Event) {
        for session in self.sessions().values() {
            session.emit(event.clone());
        }
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("settings", &"<redacted>")
            .field("sessions", &self.sessions().len())
            .finish_non_exhaustive()
    }
}

/// Player updates every few seconds and stats every minute, while the node exists.
async fn periodic_events(node: Weak<Node>) {
    let Some(interval) = node.upgrade().map(|n| n.settings.player_update_interval) else {
        return;
    };
    let mut updates = tokio::time::interval(interval);
    let mut stats = tokio::time::interval(Node::STATS_INTERVAL);
    loop {
        tokio::select! {
            _ = updates.tick() => {
                let Some(node) = node.upgrade() else { return };
                let players: Vec<_> = node.sessions().values().flat_map(|s| s.players()).collect();
                for player in players {
                    player.emit_update();
                }
            }
            _ = stats.tick() => {
                let Some(node) = node.upgrade() else { return };
                node.broadcast(&Event::Stats(node.stats()));
            }
        }
    }
}

async fn forward_youtube_session(
    node: Weak<Node>,
    mut changes: broadcast::Receiver<crate::youtube::SessionState>,
) {
    loop {
        match changes.recv().await {
            Ok(state) => match node.upgrade() {
                Some(node) => node.broadcast(&Event::YoutubeSession { state }),
                None => return,
            },
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Resident set size of this process, from `/proc/self/status`.
fn resident_memory() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}
