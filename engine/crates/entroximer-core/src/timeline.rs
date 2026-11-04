//! Time-indexed cache of demuxed packets.
//!
//! Every producer (a yt-dlp pipe, a file yt-dlp is still writing, HTTP range requests, SABR) is
//! demuxed strictly forward-only and its packets land here, keyed by presentation time. All
//! seeking happens on this structure, so it works the same for every transport and container.
//!
//! The timeline is a set of disjoint *ranges* of contiguous packets, which mirrors SABR's own
//! "buffered ranges" model. Several producers can fill different ranges; when a range grows into
//! the next one they merge and the producer that caught up is stopped.
//!
//! Ranges store their packets in *blocks* of about a second, the unit of storage and eviction.
//! Storage is tiered for RAM-constrained hosts:
//! - the block index (well under a hundred bytes per block, so a few megabytes for ten hours of
//!   audio) always lives in RAM,
//! - complete blocks are written through to anonymous temp files ("spill", on by default), one
//!   write per block,
//! - blocks keep their packets in RAM only inside a window around the playhead that reaches
//!   `forward_bias` times further ahead than behind, since listeners skip forward far more often
//!   than they skip back. A block read back from disk stays in RAM until it leaves the window.
//!
//! When a cap is hit, the unit with the largest *weighted distance* from the playhead is evicted,
//! where distance ahead counts `1 / forward_bias` as much as distance behind. If the incoming
//! packet would itself be the farthest unit, the producer is paused (backpressure) instead, so a
//! source that cannot seek by time never has to be restarted from the beginning.
//!
//! Each player reading a timeline is a *reader* with a playhead of its own, so that a repeated
//! track plays again from what it downloaded, even while it crossfades into itself. Distances are
//! measured from the nearest playhead: what any reader is near is kept.
//!
//! A timeline whose whole media is on disk can be *parked* to play again later, e.g. the next
//! time a repeated queue comes round: it leaves the shared budget and, while nobody reads it, holds
//! nothing in RAM but its block index.

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use symphonia::core::codecs::audio::AudioCodecParameters;

use crate::error::{CodecMismatch, CodecSummary, MediaError};

/// Media time in 48 kHz sample units (Discord's output rate).
pub type Ticks = i64;
pub const TICKS_PER_SECOND: Ticks = 48_000;

pub type ProducerId = u64;
pub type ReaderId = u64;

pub fn ticks(d: Duration) -> Ticks {
    (d.as_nanos() * TICKS_PER_SECOND as u128 / 1_000_000_000) as Ticks
}

pub fn duration(t: Ticks) -> Duration {
    Duration::from_nanos((t.max(0) as u128 * 1_000_000_000 / TICKS_PER_SECOND as u128) as u64)
}

/// Packets whose boundaries are closer than this are treated as contiguous or identical.
/// Container timestamps are often rounded to the millisecond, so exact equality is too strict.
fn tolerance(dur: Ticks) -> Ticks {
    (dur / 2).clamp(1, TICKS_PER_SECOND / 100)
}

/// Distance of `[start, end)` from the playhead, where distance ahead counts `1 / forward_bias`
/// as much as distance behind. Zero when the interval contains the playhead.
fn weighted_distance(playhead: Ticks, forward_bias: f64, start: Ticks, end: Ticks) -> Ticks {
    if end <= playhead {
        playhead - end
    } else if start > playhead {
        ((start - playhead) as f64 / forward_bias) as Ticks
    } else {
        0
    }
}

/// [`weighted_distance`] from the nearest of the readers' playheads; with no readers, from the
/// start of the media, where playback begins.
fn nearest_distance(readers: &[Reader], forward_bias: f64, start: Ticks, end: Ticks) -> Ticks {
    readers
        .iter()
        .map(|r| weighted_distance(r.playhead, forward_bias, start, end))
        .min()
        .unwrap_or_else(|| weighted_distance(0, forward_bias, start, end))
}

/// A producer's packet that lands this far past its own range's end starts a new range.
const MAX_HOLE: Ticks = TICKS_PER_SECOND / 10;

/// A range starting this close to zero starts where the media starts.
const MEDIA_START: Ticks = MAX_HOLE;

/// How often (in playhead movement) payloads outside the RAM window are released.
const SWEEP_INTERVAL: Ticks = TICKS_PER_SECOND;

/// A block is complete once it spans this much media time or holds this many payload bytes.
const BLOCK_SPAN: Ticks = TICKS_PER_SECOND;
const BLOCK_BYTES: usize = 64 << 10;

/// What precedes each packet's payload in a spilled block: pts, duration, trims and length.
const PACKET_HEADER: usize = 28;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub pts: Ticks,
    pub dur: Ticks,
    /// Frames (in the codec's own rate) to drop from the start/end of the decoded packet.
    pub trim_start: u32,
    pub trim_end: u32,
    pub data: Bytes,
}

impl Packet {
    pub fn end(&self) -> Ticks {
        self.pts + self.dur
    }
}

#[derive(Clone, Debug)]
pub struct RetentionConfig {
    /// How far behind the playhead payloads are kept in RAM.
    pub memory_behind: Duration,
    /// Distance ahead of the playhead counts `1 / forward_bias` as much as distance behind it,
    /// for the RAM window, RAM eviction and disk eviction alike. `4.0` with a 15 s
    /// `memory_behind` keeps 60 s ahead in RAM.
    pub forward_bias: f64,
    /// Cap on payload bytes held in RAM per timeline. With spill, the blocks being filled (one
    /// per download) and the block being played can exceed it.
    pub memory_max_bytes: usize,
    /// Disk spill; `None` keeps every payload in RAM (bounded by `memory_max_bytes`).
    pub spill: Option<SpillConfig>,
    /// Caps on what this timeline holds together with the others sharing the budget.
    pub budget: Option<Arc<CacheBudget>>,
}

#[derive(Clone, Debug)]
pub struct SpillConfig {
    pub dir: PathBuf,
    /// Cap on spilled bytes per timeline.
    pub max_bytes: u64,
    /// Size of each spill file; whole files are the unit of disk eviction.
    pub segment_bytes: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            memory_behind: Duration::from_secs(15),
            forward_bias: 4.0,
            memory_max_bytes: 8 << 20,
            spill: Some(SpillConfig::default()),
            budget: None,
        }
    }
}

/// Caps on the RAM and disk that timelines take together. Each may take an equal share, within
/// its own caps: one long track cannot crowd out the others, and short ones leave most of their
/// share unused. When a timeline joins, the others keep to their smaller shares as they play.
#[derive(Debug)]
pub struct CacheBudget {
    memory_bytes: usize,
    disk_bytes: u64,
    timelines: AtomicUsize,
}

impl CacheBudget {
    pub fn new(memory_bytes: usize, disk_bytes: u64) -> Self {
        Self {
            memory_bytes,
            disk_bytes,
            timelines: AtomicUsize::new(0),
        }
    }

    fn join(self: &Arc<Self>) -> Share {
        self.timelines.fetch_add(1, Ordering::Relaxed);
        Share(Arc::clone(self))
    }
}

/// A timeline's place in a [`CacheBudget`], given up when dropped.
#[derive(Debug)]
struct Share(Arc<CacheBudget>);

impl Share {
    fn timelines(&self) -> usize {
        self.0.timelines.load(Ordering::Relaxed).max(1)
    }

    fn memory_bytes(&self) -> usize {
        self.0.memory_bytes / self.timelines()
    }

    fn disk_bytes(&self) -> u64 {
        self.0.disk_bytes / self.timelines() as u64
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        self.0.timelines.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Default for SpillConfig {
    fn default() -> Self {
        Self {
            dir: std::env::temp_dir(),
            max_bytes: 1 << 30,
            segment_bytes: 8 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeState {
    /// A live producer is appending to the end of this range.
    Filling(ProducerId),
    /// The producer stopped (cancelled, merged, failed) before reaching the end of the media.
    Truncated,
    /// The range ends where the media ends.
    Complete,
}

#[derive(Debug)]
pub enum PushResult {
    Continue,
    /// The producer is no longer needed: it was cancelled, caught up with buffered data, or is
    /// duplicating another producer. Its range (if any) has already been taken care of.
    Stop,
}

#[derive(Debug)]
pub enum Finish {
    /// The producer delivered everything up to the end of the media.
    Eof,
    /// The producer was cancelled or is no longer needed.
    Stopped,
    Failed(MediaError),
}

#[derive(Debug)]
pub enum Lookup {
    Ready(Packet),
    Pending(Pending),
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    /// `t` lies past the end of a range that `producer` is still filling.
    Frontier {
        producer: ProducerId,
        /// Where `producer` was asked to start.
        opened_at: Ticks,
        frontier: Ticks,
    },
    /// A producer was opened at or before `t` but has not delivered packets yet.
    Opening { producer: ProducerId },
    /// Nothing buffered and nothing on the way.
    Gap,
}

#[derive(Clone, Debug, Default)]
pub struct TimelineStats {
    pub ranges: usize,
    pub packets: usize,
    pub memory_bytes: usize,
    pub disk_bytes: u64,
}

pub struct Timeline {
    inner: Mutex<Inner>,
    changed: Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiskLoc {
    segment: u64,
    offset: u64,
    len: u32,
}

/// Consecutive packets of one range, stored and evicted together. Only the last block of a
/// range that a producer is filling is still open for packets.
struct Block {
    /// Where the first packet starts.
    start: Ticks,
    /// Where the last packet ends.
    end: Ticks,
    packets: u32,
    /// Payload bytes: what holding the block in RAM costs.
    bytes: usize,
    /// The packets, while the block is held in RAM.
    mem: Option<Vec<Packet>>,
    /// Where the block was spilled to, once complete.
    disk: Option<DiskLoc>,
    /// Complete: nothing more is appended.
    sealed: bool,
}

impl Block {
    fn open(pkt: Packet) -> Self {
        Self {
            start: pkt.pts,
            end: pkt.end(),
            packets: 1,
            bytes: pkt.data.len(),
            mem: Some(vec![pkt]),
            disk: None,
            sealed: false,
        }
    }

    fn push(&mut self, pkt: Packet) {
        self.end = self.end.max(pkt.end());
        self.packets += 1;
        self.bytes += pkt.data.len();
        self.mem
            .as_mut()
            .expect("open blocks are held in RAM")
            .push(pkt);
    }

    fn is_full(&self) -> bool {
        self.end - self.start >= BLOCK_SPAN || self.bytes >= BLOCK_BYTES
    }

    /// The space the block takes once spilled.
    fn spilled_len(&self) -> u64 {
        (self.bytes + self.packets as usize * PACKET_HEADER) as u64
    }
}

/// Never empty; ranges are kept sorted by start and do not overlap (beyond `tolerance`).
struct Range {
    blocks: VecDeque<Block>,
    state: RangeState,
}

impl Range {
    fn start(&self) -> Ticks {
        self.blocks.front().map_or(0, |b| b.start)
    }

    fn end(&self) -> Ticks {
        self.blocks.back().map_or(0, |b| b.end)
    }

    /// Index of the first block that ends after `t`.
    fn block_at(&self, t: Ticks) -> usize {
        self.blocks.partition_point(|b| b.end <= t)
    }
}

struct Segment {
    id: u64,
    file: Arc<File>,
    len: u64,
    start: Ticks,
    end: Ticks,
}

struct ProducerState {
    requested_start: Ticks,
    first: Option<(Instant, Ticks)>,
    frontier: Ticks,
    cancelled: bool,
    segment: Option<u64>,
}

struct Reader {
    id: ReaderId,
    playhead: Ticks,
    /// The playhead at the last sweep of RAM it caused.
    last_sweep: Ticks,
}

struct Inner {
    cfg: RetentionConfig,
    codec: Option<AudioCodecParameters>,
    ranges: Vec<Range>,
    producers: HashMap<ProducerId, ProducerState>,
    next_producer: ProducerId,
    segments: Vec<Segment>,
    next_segment: u64,
    spill_failed: bool,
    memory_bytes: usize,
    disk_bytes: u64,
    share: Option<Share>,
    readers: Vec<Reader>,
    next_reader: ReaderId,
    media_end: Option<Ticks>,
    failure: Option<MediaError>,
    /// Sealed blocks with room reserved on disk, to be written without the lock held.
    unwritten: Vec<Unwritten>,
    /// Kept for playing later; see [`Timeline::park`].
    parked: bool,
}

/// A sealed block on its way to disk. It stays in RAM until it is written and attached.
struct Unwritten {
    start: Ticks,
    end: Ticks,
    loc: DiskLoc,
    file: Arc<File>,
    data: Vec<u8>,
}

enum Located {
    Inside(usize),
    AtEnd(usize),
    Outside,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RangeEnd {
    Front,
    Back,
}

enum Probe {
    Buffered { range: usize, block: usize },
    Pending(Pending),
    End,
}

impl std::fmt::Debug for Timeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timeline")
            .field("buffered", &self.buffered())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl Timeline {
    pub fn new(cfg: RetentionConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                share: cfg.budget.as_ref().map(CacheBudget::join),
                cfg,
                codec: None,
                ranges: Vec::new(),
                producers: HashMap::new(),
                next_producer: 1,
                segments: Vec::new(),
                next_segment: 0,
                spill_failed: false,
                memory_bytes: 0,
                disk_bytes: 0,
                readers: Vec::new(),
                next_reader: 1,
                media_end: None,
                failure: None,
                unwritten: Vec::new(),
                parked: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a producer that will deliver packets starting at (or shortly before)
    /// `requested_start`; `None` means the beginning of the media.
    pub fn register_producer(&self, requested_start: Option<Ticks>) -> ProducerId {
        let mut g = self.lock();
        let id = g.next_producer;
        g.next_producer += 1;
        g.producers.insert(
            id,
            ProducerState {
                requested_start: requested_start.unwrap_or(0),
                first: None,
                frontier: requested_start.unwrap_or(0),
                cancelled: false,
                segment: None,
            },
        );
        id
    }

    /// Pins the codec on first call; later producers must deliver the same codec.
    pub fn set_codec(&self, params: &AudioCodecParameters) -> Result<(), CodecMismatch> {
        let mut g = self.lock();
        let Some(pinned) = &g.codec else {
            g.codec = Some(params.clone());
            drop(g);
            self.changed.notify_all();
            return Ok(());
        };
        let (expected, found) = (CodecSummary::from(pinned), CodecSummary::from(params));
        if expected == found {
            Ok(())
        } else {
            Err(CodecMismatch { expected, found })
        }
    }

    pub fn codec(&self) -> Option<AudioCodecParameters> {
        self.lock().codec.clone()
    }

    /// Adds a packet, blocking while the cache is full and this packet would be the first thing
    /// evicted (backpressure).
    pub fn push(&self, producer: ProducerId, pkt: Packet) -> PushResult {
        let mut g = self.lock();
        loop {
            match g.producers.get(&producer) {
                Some(p) if !p.cancelled => {}
                _ => return PushResult::Stop,
            }
            if !g.must_wait_for_space(&pkt) {
                break;
            }
            g = self
                .changed
                .wait_timeout(g, Duration::from_millis(250))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let result = g.insert(producer, pkt);
        g.enforce_caps();
        let unwritten = std::mem::take(&mut g.unwritten);
        drop(g);
        self.changed.notify_all();
        self.write(unwritten);
        result
    }

    pub fn finish(&self, producer: ProducerId, outcome: Finish) {
        let mut g = self.lock();
        let Some(cancelled) = g.producers.get(&producer).map(|p| p.cancelled) else {
            let unwritten = std::mem::take(&mut g.unwritten);
            drop(g);
            self.write(unwritten);
            return;
        };
        // a cancelled producer's stream ends early; that is not the end of the media
        let outcome = match outcome {
            Finish::Eof if cancelled => Finish::Stopped,
            other => other,
        };
        let range = g.filled_by(producer);
        match &outcome {
            Finish::Eof => {
                let end = match range {
                    Some(i) => {
                        g.end_filling(i, RangeState::Complete);
                        g.ranges[i].end()
                    }
                    // nothing delivered: the producer was opened at or past the end
                    None => g.producers[&producer].requested_start,
                };
                g.media_end = Some(g.media_end.map_or(end, |e| e.min(end)));
            }
            Finish::Stopped | Finish::Failed(_) => {
                if let Some(i) = range {
                    g.end_filling(i, RangeState::Truncated);
                }
            }
        }
        g.producers.remove(&producer);
        if let Finish::Failed(error) = outcome {
            g.failure = Some(error);
        }
        g.enforce_caps();
        // with what a cancel sealed: cancels come from wherever producers are dropped, which may
        // be the audio thread, and this from the producer's own task
        let unwritten = std::mem::take(&mut g.unwritten);
        drop(g);
        self.changed.notify_all();
        self.write(unwritten);
    }

    /// Writes sealed blocks to the room reserved for them, without holding the lock: a write
    /// can wait on the disk (e.g. while the kernel throttles dirty pages), and the lock is what
    /// rendering takes. Each block is attached to its place on disk once written.
    fn write(&self, unwritten: Vec<Unwritten>) {
        for block in unwritten {
            let written = block.file.write_all_at(&block.data, block.loc.offset);
            let mut g = self.lock();
            match written {
                Ok(()) => g.attach(block),
                Err(e) => {
                    if !g.spill_failed {
                        tracing::warn!("disabling disk spill: {e}");
                    }
                    g.spill_failed = true;
                }
            }
        }
    }

    /// Stops accepting packets from `producer`; its range becomes `Truncated`.
    pub fn cancel(&self, producer: ProducerId) {
        let mut g = self.lock();
        if let Some(p) = g.producers.get_mut(&producer) {
            p.cancelled = true;
        }
        if let Some(i) = g.filled_by(producer) {
            g.end_filling(i, RangeState::Truncated);
            g.enforce_caps();
        }
        drop(g);
        self.changed.notify_all();
    }

    /// The packet covering `t`, or why there is none. Reads spilled blocks from disk.
    pub fn lookup(&self, t: Ticks) -> io::Result<Lookup> {
        let (file, loc) = {
            let g = self.lock();
            let (range, block) = match g.probe(t) {
                Probe::Buffered { range, block } => (range, block),
                Probe::Pending(pending) => return Ok(Lookup::Pending(pending)),
                Probe::End => return Ok(Lookup::End),
            };
            let block = &g.ranges[range].blocks[block];
            if let Some(packets) = &block.mem {
                return Ok(Lookup::Ready(packet_at(packets, t).clone()));
            }
            let loc = block
                .disk
                .expect("blocks without their packets in RAM are on disk");
            (g.segment_file(loc.segment), loc)
        };

        // read without the lock; the file stays readable even if its segment is evicted meanwhile
        let mut buf = vec![0; loc.len as usize];
        file.read_exact_at(&mut buf, loc.offset)?;
        let packets = decode_block(Bytes::from(buf))?;
        let packet = packet_at(&packets, t).clone();
        self.lock().keep_in_memory(t, loc, packets);
        Ok(Lookup::Ready(packet))
    }

    /// What is missing at `t`; `None` if `t` is buffered or past the end. Never touches disk.
    pub fn pending_at(&self, t: Ticks) -> Option<Pending> {
        match self.lock().probe(t) {
            Probe::Pending(pending) => Some(pending),
            Probe::Buffered { .. } | Probe::End => None,
        }
    }

    /// Adds a reader whose playback starts at `at`.
    pub fn add_reader(&self, at: Ticks) -> ReaderId {
        let mut g = self.lock();
        let id = g.next_reader;
        g.next_reader += 1;
        g.readers.push(Reader {
            id,
            playhead: at,
            last_sweep: at,
        });
        id
    }

    /// Stops keeping media for a reader that is done.
    pub fn remove_reader(&self, reader: ReaderId) {
        let mut g = self.lock();
        g.readers.retain(|r| r.id != reader);
        g.release_unread();
        drop(g);
        // what it held may now make room for paused producers
        self.changed.notify_all();
    }

    /// Informs retention of a reader's playback position; wakes paused producers.
    pub fn set_playhead(&self, reader: ReaderId, t: Ticks) {
        let mut g = self.lock();
        let Some(r) = g.readers.iter_mut().find(|r| r.id == reader) else {
            return;
        };
        r.playhead = t;
        if (t - r.last_sweep).abs() >= SWEEP_INTERVAL {
            r.last_sweep = t;
            g.sweep_memory();
            g.enforce_caps();
        }
        drop(g);
        self.changed.notify_all();
    }

    /// Blocks until something changes (packet pushed, producer finished, ...) or `timeout`.
    pub fn wait(&self, timeout: Duration) {
        let g = self.lock();
        let _ = self.changed.wait_timeout(g, timeout);
    }

    /// Download speed of a producer in media seconds per wall-clock second.
    pub fn producer_rate(&self, producer: ProducerId) -> Option<f64> {
        let g = self.lock();
        let p = g.producers.get(&producer)?;
        let (since, first_pts) = p.first?;
        let elapsed = since.elapsed().as_secs_f64();
        if elapsed < 0.25 {
            return None;
        }
        Some((p.frontier - first_pts) as f64 / TICKS_PER_SECOND as f64 / elapsed)
    }

    pub fn live_producers(&self) -> Vec<ProducerId> {
        let g = self.lock();
        let mut ids: Vec<_> = g
            .producers
            .iter()
            .filter(|(_, p)| !p.cancelled)
            .map(|(&id, _)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Buffered `(start, end)` intervals, in order.
    pub fn buffered(&self) -> Vec<(Ticks, Ticks)> {
        self.lock()
            .ranges
            .iter()
            .map(|r| (r.start(), r.end()))
            .collect()
    }

    /// The range containing `t`, as `(start, end, state)`.
    pub fn range_at(&self, t: Ticks) -> Option<(Ticks, Ticks, RangeState)> {
        self.lock()
            .ranges
            .iter()
            .find(|r| r.start() <= t && t < r.end())
            .map(|r| (r.start(), r.end(), r.state))
    }

    pub fn media_end(&self) -> Option<Ticks> {
        self.lock().media_end
    }

    /// The bytes on disk, if the whole media is there: one complete range from the start of the
    /// media, every block of it spilled. `None` while it downloads, without spill, or once some
    /// of it was evicted.
    pub fn complete_on_disk(&self) -> Option<u64> {
        self.lock().complete_on_disk()
    }

    /// Keeps the media for playing later, if it is [complete on disk](Self::complete_on_disk):
    /// the timeline leaves the shared budget, so that it no longer takes a share from those
    /// playing (its own caps still apply), and holds no payloads in RAM while nobody reads it.
    /// Returns its bytes on disk.
    pub fn park(&self) -> Option<u64> {
        let mut g = self.lock();
        let bytes = g.complete_on_disk()?;
        g.parked = true;
        g.share = None;
        g.release_unread();
        Some(bytes)
    }

    /// The most recent producer failure, if any, which is cleared by this call.
    pub fn take_failure(&self) -> Option<MediaError> {
        self.lock().failure.take()
    }

    pub fn stats(&self) -> TimelineStats {
        let g = self.lock();
        TimelineStats {
            ranges: g.ranges.len(),
            packets: g
                .ranges
                .iter()
                .flat_map(|r| &r.blocks)
                .map(|b| b.packets as usize)
                .sum(),
            memory_bytes: g.memory_bytes,
            disk_bytes: g.disk_bytes,
        }
    }
}

impl Inner {
    fn complete_on_disk(&self) -> Option<u64> {
        let [range] = self.ranges.as_slice() else {
            return None;
        };
        let complete = range.state == RangeState::Complete
            && range.start() <= MEDIA_START
            && self.unwritten.is_empty()
            && range.blocks.iter().all(|b| b.sealed && b.disk.is_some());
        complete.then_some(self.disk_bytes)
    }

    /// A parked timeline that nobody reads holds no payloads in RAM: they are all on disk.
    fn release_unread(&mut self) {
        if !self.parked || !self.readers.is_empty() {
            return;
        }
        let mut freed = 0;
        for b in self.ranges.iter_mut().flat_map(|r| r.blocks.iter_mut()) {
            if b.disk.is_some() && b.mem.take().is_some() {
                freed += b.bytes;
            }
        }
        self.memory_bytes -= freed;
    }

    fn probe(&self, t: Ticks) -> Probe {
        if self.media_end.is_some_and(|end| t >= end) {
            return Probe::End;
        }

        let mut frontier = None;
        let preceding = self.ranges.partition_point(|r| r.start() <= t + MAX_HOLE);
        if let Some(range) = preceding.checked_sub(1) {
            let r = &self.ranges[range];
            let block = r.block_at(t);
            if block < r.blocks.len() {
                return Probe::Buffered { range, block };
            }
            match r.state {
                RangeState::Filling(producer) => frontier = Some((producer, r.end())),
                RangeState::Complete => return Probe::End,
                RangeState::Truncated => {}
            }
        }

        // a producer opened between the frontier (if any) and `t` will get there first
        let opening = self
            .producers
            .iter()
            .filter(|(_, p)| !p.cancelled && p.first.is_none())
            .filter(|(_, p)| p.requested_start <= t + MAX_HOLE)
            .filter(|(_, p)| frontier.is_none_or(|(_, f)| p.requested_start > f))
            .max_by_key(|(_, p)| p.requested_start)
            .map(|(&id, _)| id);

        Probe::Pending(match (opening, frontier) {
            (Some(producer), _) => Pending::Opening { producer },
            (None, Some((producer, frontier))) => Pending::Frontier {
                producer,
                opened_at: self.producers[&producer].requested_start,
                frontier,
            },
            (None, None) => Pending::Gap,
        })
    }

    fn segment_file(&self, id: u64) -> Arc<File> {
        let segment = self
            .segments
            .iter()
            .find(|s| s.id == id)
            .expect("blocks never point at evicted segments");
        Arc::clone(&segment.file)
    }

    /// Keeps a block read back from disk in RAM for the reads of its other packets, until a
    /// sweep finds it outside the RAM window.
    fn keep_in_memory(&mut self, t: Ticks, loc: DiskLoc, packets: Vec<Packet>) {
        let Probe::Buffered { range, block } = self.probe(t) else {
            return;
        };
        let block = &mut self.ranges[range].blocks[block];
        if block.mem.is_some() || block.disk != Some(loc) {
            return; // evicted or read by someone else meanwhile
        }
        block.mem = Some(packets);
        self.memory_bytes += block.bytes;
        if self.memory_bytes > self.memory_cap() {
            self.sweep_memory();
        }
    }

    fn spill_active(&self) -> bool {
        self.cfg.spill.is_some() && !self.spill_failed
    }

    /// Cap on payload bytes in RAM: the timeline's own, or its share of the budget if smaller.
    fn memory_cap(&self) -> usize {
        let share = self.share.as_ref().map_or(usize::MAX, Share::memory_bytes);
        self.cfg.memory_max_bytes.min(share)
    }

    /// Cap on spilled bytes, like [`Self::memory_cap`].
    fn disk_cap(&self) -> u64 {
        let own = self.cfg.spill.as_ref().map_or(u64::MAX, |s| s.max_bytes);
        own.min(self.share.as_ref().map_or(u64::MAX, Share::disk_bytes))
    }

    fn weight(&self, start: Ticks, end: Ticks) -> Ticks {
        nearest_distance(&self.readers, self.cfg.forward_bias, start, end)
    }

    fn in_memory_window(&self, start: Ticks, end: Ticks) -> bool {
        self.weight(start, end) <= ticks(self.cfg.memory_behind)
    }

    /// Whether `extra` more payload bytes would exceed the active tier's cap. Open blocks count
    /// with the space they will take on disk.
    fn over_cap(&self, extra: usize) -> bool {
        if self.spill_active() {
            let open: u64 = self
                .ranges
                .iter()
                .filter_map(|r| r.blocks.back())
                .filter(|b| !b.sealed)
                .map(Block::spilled_len)
                .sum();
            self.disk_bytes + open + (extra + PACKET_HEADER) as u64 > self.disk_cap()
        } else {
            self.memory_bytes + extra > self.memory_cap()
        }
    }

    /// The farthest evictable unit for the active tier: a spill segment, or a range endpoint.
    fn farthest(&self) -> Option<Ticks> {
        let w = if self.spill_active() {
            self.segments
                .iter()
                .map(|s| self.weight(s.start, s.end))
                .max()
        } else {
            self.ranges
                .iter()
                .flat_map(|r| [r.blocks.front(), r.blocks.back()])
                .flatten()
                .map(|b| self.weight(b.start, b.end))
                .max()
        };
        w.filter(|&w| w > 0)
    }

    fn must_wait_for_space(&self, pkt: &Packet) -> bool {
        if !self.over_cap(pkt.data.len()) {
            return false;
        }
        let w = self.weight(pkt.pts, pkt.end());
        // a packet at the playhead always goes in; with nothing evictable we overflow rather
        // than deadlock
        w > 0 && self.farthest().is_some_and(|f| w >= f)
    }

    fn locate(&self, t: Ticks, tol: Ticks) -> Located {
        for (i, r) in self.ranges.iter().enumerate() {
            if t >= r.start() - tol && t < r.end() - tol {
                return Located::Inside(i);
            }
            if (t - r.end()).abs() <= tol {
                return Located::AtEnd(i);
            }
        }
        Located::Outside
    }

    /// The range `producer` is appending to.
    fn filled_by(&self, producer: ProducerId) -> Option<usize> {
        self.ranges
            .iter()
            .position(|r| r.state == RangeState::Filling(producer))
    }

    fn insert(&mut self, producer: ProducerId, pkt: Packet) -> PushResult {
        let p = self
            .producers
            .get_mut(&producer)
            .expect("checked by caller");
        p.first.get_or_insert_with(|| (Instant::now(), pkt.pts));
        p.frontier = p.frontier.max(pkt.end());
        let tol = tolerance(pkt.dur);

        let idx = match self.filled_by(producer) {
            Some(i) if pkt.pts < self.ranges[i].end() - tol => return PushResult::Continue,
            Some(i) if pkt.pts > self.ranges[i].end() + MAX_HOLE => {
                // our tail was evicted (or the stream jumped): continue in a new range
                self.end_filling(i, RangeState::Truncated);
                return self.insert_new_range(producer, pkt);
            }
            Some(i) => i,
            None => match self.locate(pkt.pts, tol) {
                Located::Inside(i) => {
                    return if self.ranges[i].state == RangeState::Truncated {
                        // re-filling an interrupted range: skip what it already has
                        self.ranges[i].state = RangeState::Filling(producer);
                        PushResult::Continue
                    } else {
                        // complete, or another producer is already ahead of us
                        PushResult::Stop
                    };
                }
                Located::AtEnd(i) if self.ranges[i].state == RangeState::Truncated => {
                    self.ranges[i].state = RangeState::Filling(producer);
                    i
                }
                Located::AtEnd(_) => return PushResult::Stop,
                Located::Outside => return self.insert_new_range(producer, pkt),
            },
        };

        self.memory_bytes += pkt.data.len();
        let range = &mut self.ranges[idx];
        match range.blocks.back_mut() {
            Some(open) if !open.sealed => open.push(pkt),
            _ => range.blocks.push_back(Block::open(pkt)),
        }
        self.seal_if_full(idx, producer);
        self.try_merge(idx, producer, tol)
    }

    fn insert_new_range(&mut self, producer: ProducerId, pkt: Packet) -> PushResult {
        let tol = tolerance(pkt.dur);
        let start = pkt.pts;
        self.memory_bytes += pkt.data.len();
        let idx = self.ranges.partition_point(|r| r.start() < start);
        self.ranges.insert(
            idx,
            Range {
                blocks: VecDeque::from([Block::open(pkt)]),
                state: RangeState::Filling(producer),
            },
        );
        self.seal_if_full(idx, producer);
        self.try_merge(idx, producer, tol)
    }

    /// Joins range `idx` with its successor once they touch; the producer filling `idx` stops.
    fn try_merge(&mut self, idx: usize, producer: ProducerId, tol: Ticks) -> PushResult {
        if idx + 1 >= self.ranges.len()
            || self.ranges[idx].end() + tol < self.ranges[idx + 1].start()
        {
            return PushResult::Continue;
        }
        self.seal_tail(idx, producer);
        let next = self.ranges.remove(idx + 1);
        let end = self.ranges[idx].end();
        for block in next.blocks {
            if block.end > end + tol {
                self.ranges[idx].blocks.push_back(block);
            } else if block.mem.is_some() {
                // everything in it is here already
                self.memory_bytes -= block.bytes;
            }
        }
        self.ranges[idx].state = next.state;
        PushResult::Stop
    }

    /// The producer filling range `idx` is done with it.
    fn end_filling(&mut self, idx: usize, state: RangeState) {
        if let RangeState::Filling(producer) = self.ranges[idx].state {
            self.seal_tail(idx, producer);
        }
        self.ranges[idx].state = state;
    }

    fn seal_if_full(&mut self, idx: usize, producer: ProducerId) {
        if self.ranges[idx].blocks.back().is_some_and(Block::is_full) {
            self.seal_tail(idx, producer);
        }
    }

    /// Completes the open block at the end of range `idx`, if any. When spilling, room is
    /// reserved for it on disk and it is queued for [`Timeline::write`]; it stays in RAM until
    /// then.
    fn seal_tail(&mut self, idx: usize, producer: ProducerId) {
        let spill = self.spill_active();
        let Some(block) = self.ranges[idx].blocks.back_mut().filter(|b| !b.sealed) else {
            return;
        };
        block.sealed = true;
        if !spill {
            return;
        }
        let (start, end) = (block.start, block.end);
        let data = encode_block(block.mem.as_deref().expect("open blocks are held in RAM"));
        if let Some((loc, file)) = self.reserve_disk(producer, data.len() as u64, start, end) {
            self.unwritten.push(Unwritten {
                start,
                end,
                loc,
                file,
                data,
            });
        }
        // otherwise spilling failed, and the block stays in RAM
    }

    /// Reserves `len` bytes in `producer`'s spill segment, starting a new one when it is full.
    fn reserve_disk(
        &mut self,
        producer: ProducerId,
        len: u64,
        start: Ticks,
        end: Ticks,
    ) -> Option<(DiskLoc, Arc<File>)> {
        let spill = self.cfg.spill.as_ref().expect("spill is active");
        let current = self
            .producers
            .get(&producer)
            .and_then(|p| p.segment)
            .and_then(|id| self.segments.iter().position(|s| s.id == id))
            .filter(|&i| self.segments[i].len + len <= spill.segment_bytes);
        let si = match current {
            Some(i) => i,
            // creating a file is rare and quick, unlike writing to one
            None => match tempfile::tempfile_in(&spill.dir) {
                Ok(file) => {
                    let id = self.next_segment;
                    self.next_segment += 1;
                    self.segments.push(Segment {
                        id,
                        file: Arc::new(file),
                        len: 0,
                        start,
                        end,
                    });
                    if let Some(p) = self.producers.get_mut(&producer) {
                        p.segment = Some(id);
                    }
                    self.segments.len() - 1
                }
                Err(e) => {
                    tracing::warn!(dir = %spill.dir.display(), "disabling disk spill: {e}");
                    self.spill_failed = true;
                    return None;
                }
            },
        };
        let seg = &mut self.segments[si];
        let loc = DiskLoc {
            segment: seg.id,
            offset: seg.len,
            len: u32::try_from(len).expect("blocks are far smaller than 4 GiB"),
        };
        seg.len += len;
        seg.start = seg.start.min(start);
        seg.end = seg.end.max(end);
        self.disk_bytes += len;
        Some((loc, Arc::clone(&seg.file)))
    }

    /// Points a written block at its place on disk, keeping its packets in RAM only inside the
    /// RAM window and the RAM cap.
    fn attach(&mut self, written: Unwritten) {
        let Unwritten {
            start,
            end,
            loc,
            data,
            ..
        } = written;
        let is_it = |b: &Block| b.start == start && b.end == end && b.sealed && b.disk.is_none();
        let Probe::Buffered { range, block } = self.probe(start) else {
            return; // evicted meanwhile
        };
        if !is_it(&self.ranges[range].blocks[block]) {
            return;
        }
        if !self.segments.iter().any(|s| s.id == loc.segment) {
            // its segment was evicted meanwhile, which takes the segment's blocks with it
            self.retain_blocks(|b| !is_it(b));
            return;
        }
        let keep = self.in_memory_window(start, end) && self.memory_bytes <= self.memory_cap();
        let block = &mut self.ranges[range].blocks[block];
        block.disk = Some(loc);
        if keep {
            // one allocation for the whole block instead of one per packet
            block.mem = Some(decode_block(Bytes::from(data)).expect("just encoded"));
        } else {
            block.mem = None;
            self.memory_bytes -= block.bytes;
        }
    }

    /// Releases RAM copies of blocks that are safely on disk and outside the RAM window, and the
    /// farthest others while over the RAM cap.
    fn sweep_memory(&mut self) {
        if !self.spill_active() {
            return;
        }
        let behind = ticks(self.cfg.memory_behind);
        let (readers, bias) = (&self.readers, self.cfg.forward_bias);
        let weight = |b: &Block| nearest_distance(readers, bias, b.start, b.end);
        let mut freed = 0;
        for b in self.ranges.iter_mut().flat_map(|r| r.blocks.iter_mut()) {
            if b.disk.is_some() && weight(b) > behind && b.mem.take().is_some() {
                freed += b.bytes;
            }
        }
        self.memory_bytes -= freed;

        if self.memory_bytes > self.memory_cap() {
            let mut held: Vec<(Ticks, usize, usize)> = Vec::new();
            for (ri, r) in self.ranges.iter().enumerate() {
                for (bi, b) in r.blocks.iter().enumerate() {
                    if b.mem.is_some() && b.disk.is_some() {
                        held.push((weight(b), ri, bi));
                    }
                }
            }
            held.sort_unstable_by_key(|&(weight, ..)| Reverse(weight));
            for (_, ri, bi) in held {
                if self.memory_bytes <= self.memory_cap() {
                    break;
                }
                let block = &mut self.ranges[ri].blocks[bi];
                if block.mem.take().is_some() {
                    self.memory_bytes -= block.bytes;
                }
            }
        }
    }

    fn enforce_caps(&mut self) {
        if self.spill_active() {
            // RAM is kept in check where blocks enter it, and by sweeps as the playhead moves
            let max = self.disk_cap();
            while self.disk_bytes > max {
                let Some(id) = self
                    .segments
                    .iter()
                    .map(|s| (self.weight(s.start, s.end), s.id))
                    .filter(|&(w, _)| w > 0)
                    .max()
                    .map(|(_, id)| id)
                else {
                    break;
                };
                self.evict_segment(id);
            }
        } else {
            while self.memory_bytes > self.memory_cap() {
                if !self.evict_farthest_endpoint() {
                    break;
                }
            }
        }
    }

    /// Deletes a spill file and every block in it.
    fn evict_segment(&mut self, id: u64) {
        let Some(pos) = self.segments.iter().position(|s| s.id == id) else {
            return;
        };
        let seg = self.segments.remove(pos);
        self.disk_bytes -= seg.len;
        for p in self.producers.values_mut() {
            if p.segment == Some(id) {
                p.segment = None;
            }
        }
        self.retain_blocks(|b| b.disk.is_none_or(|d| d.segment != id));
    }

    /// Drops the block at whichever range end is farthest from the playheads.
    fn evict_farthest_endpoint(&mut self) -> bool {
        let (readers, bias) = (&self.readers, self.cfg.forward_bias);
        let weight = |b: &Block| nearest_distance(readers, bias, b.start, b.end);
        let farthest = self
            .ranges
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                let front = r.blocks.front().map(|b| (b, RangeEnd::Front));
                let back = r.blocks.back().map(|b| (b, RangeEnd::Back));
                front
                    .into_iter()
                    .chain(back)
                    .map(move |(b, end)| (weight(b), i, end))
            })
            .filter(|&(weight, ..)| weight > 0)
            .max_by_key(|&(weight, ..)| weight);
        let Some((_, i, end)) = farthest else {
            return false;
        };

        let range = &mut self.ranges[i];
        let block = match end {
            RangeEnd::Front => range.blocks.pop_front(),
            RangeEnd::Back => range.blocks.pop_back(),
        }
        .expect("ranges are never empty");
        if range.blocks.is_empty() {
            self.ranges.remove(i);
        } else if end == RangeEnd::Back && range.state == RangeState::Complete {
            // the end of the media is no longer buffered
            range.state = RangeState::Truncated;
        }
        if block.mem.is_some() {
            self.memory_bytes -= block.bytes;
        }
        true
    }

    /// Removes blocks failing `keep`, splitting ranges around the holes. Only the run that still
    /// reaches the original end keeps the range's state; every other run is `Truncated`.
    fn retain_blocks(&mut self, keep: impl Fn(&Block) -> bool) {
        let mut out = Vec::with_capacity(self.ranges.len());
        for r in std::mem::take(&mut self.ranges) {
            let mut runs: Vec<VecDeque<Block>> = Vec::new();
            let mut cur = VecDeque::new();
            for b in r.blocks {
                if keep(&b) {
                    cur.push_back(b);
                } else {
                    if b.mem.is_some() {
                        self.memory_bytes -= b.bytes;
                    }
                    if !cur.is_empty() {
                        runs.push(std::mem::take(&mut cur));
                    }
                }
            }
            let tail_kept = !cur.is_empty();
            if tail_kept {
                runs.push(cur);
            }
            let last = runs.len().saturating_sub(1);
            for (k, blocks) in runs.into_iter().enumerate() {
                let state = if k == last && tail_kept {
                    r.state
                } else {
                    RangeState::Truncated
                };
                out.push(Range { blocks, state });
            }
        }
        self.ranges = out;
    }
}

/// The packet covering `t` in a block that ends after `t`, or the first one after `t` when the
/// block starts later.
fn packet_at(packets: &[Packet], t: Ticks) -> &Packet {
    let i = packets.partition_point(|p| p.end() <= t);
    &packets[i.min(packets.len() - 1)]
}

/// A spilled block: its packets one after another, each a fixed header followed by its payload.
fn encode_block(packets: &[Packet]) -> Vec<u8> {
    let len = packets.iter().map(|p| PACKET_HEADER + p.data.len()).sum();
    let mut buf = Vec::with_capacity(len);
    for p in packets {
        let data_len = u32::try_from(p.data.len()).expect("packets are far smaller than 4 GiB");
        buf.extend_from_slice(&p.pts.to_le_bytes());
        buf.extend_from_slice(&p.dur.to_le_bytes());
        buf.extend_from_slice(&p.trim_start.to_le_bytes());
        buf.extend_from_slice(&p.trim_end.to_le_bytes());
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.extend_from_slice(&p.data);
    }
    buf
}

/// The packets of a spilled block; their payloads share `buf`.
fn decode_block(mut buf: Bytes) -> io::Result<Vec<Packet>> {
    let corrupt = || io::Error::new(io::ErrorKind::InvalidData, "a spilled block is corrupt");
    let mut packets = Vec::new();
    while buf.has_remaining() {
        if buf.remaining() < PACKET_HEADER {
            return Err(corrupt());
        }
        let (pts, dur) = (buf.get_i64_le(), buf.get_i64_le());
        let (trim_start, trim_end) = (buf.get_u32_le(), buf.get_u32_le());
        let len = buf.get_u32_le() as usize;
        if buf.remaining() < len {
            return Err(corrupt());
        }
        packets.push(Packet {
            pts,
            dur,
            trim_start,
            trim_end,
            data: buf.split_to(len),
        });
    }
    if packets.is_empty() {
        return Err(corrupt());
    }
    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PKT: Ticks = 960; // 20 ms

    fn pkt(i: i64) -> Packet {
        Packet {
            pts: i * PKT,
            dur: PKT,
            trim_start: 0,
            trim_end: 0,
            data: Bytes::from(vec![(i % 251) as u8; 400]),
        }
    }

    /// Number of 20 ms packets in `s` seconds.
    fn packets_in(s: i64) -> i64 {
        s * TICKS_PER_SECOND / PKT
    }

    fn memory_only() -> RetentionConfig {
        RetentionConfig {
            spill: None,
            ..Default::default()
        }
    }

    fn spill_into(dir: &tempfile::TempDir, max_bytes: u64, segment_bytes: u64) -> RetentionConfig {
        RetentionConfig {
            spill: Some(SpillConfig {
                dir: dir.path().to_owned(),
                max_bytes,
                segment_bytes,
            }),
            ..Default::default()
        }
    }

    fn fill(tl: &Timeline, p: ProducerId, range: std::ops::Range<i64>) -> PushResult {
        for i in range {
            if let PushResult::Stop = tl.push(p, pkt(i)) {
                return PushResult::Stop;
            }
        }
        PushResult::Continue
    }

    fn ready(tl: &Timeline, t: Ticks) -> Packet {
        match tl.lookup(t).unwrap() {
            Lookup::Ready(p) => p,
            other => panic!("expected packet at {t}, got {other:?}"),
        }
    }

    #[test]
    fn lookup_contiguous_then_end() {
        let tl = Timeline::new(memory_only());
        let p = tl.register_producer(None);
        fill(&tl, p, 0..100);
        assert_eq!(ready(&tl, 0), pkt(0));
        assert_eq!(ready(&tl, 50 * PKT + 10), pkt(50));
        assert!(matches!(
            tl.lookup(100 * PKT).unwrap(),
            Lookup::Pending(Pending::Frontier { frontier, .. }) if frontier == 100 * PKT
        ));
        tl.finish(p, Finish::Eof);
        assert!(matches!(tl.lookup(100 * PKT).unwrap(), Lookup::End));
        assert_eq!(tl.media_end(), Some(100 * PKT));
    }

    #[test]
    fn gap_is_filled_and_merged() {
        let tl = Timeline::new(memory_only());
        let a = tl.register_producer(None);
        fill(&tl, a, 0..100);
        tl.cancel(a);
        let b = tl.register_producer(Some(200 * PKT));
        fill(&tl, b, 200..300);
        assert_eq!(tl.buffered(), vec![(0, 100 * PKT), (200 * PKT, 300 * PKT)]);
        assert!(matches!(
            tl.lookup(150 * PKT).unwrap(),
            Lookup::Pending(Pending::Gap)
        ));

        // a third producer re-fills from inside the truncated range, adopts it and merges into
        // the next one once they meet
        let c = tl.register_producer(Some(90 * PKT));
        assert!(matches!(
            tl.lookup(150 * PKT).unwrap(),
            Lookup::Pending(Pending::Opening { producer }) if producer == c
        ));
        assert!(matches!(fill(&tl, c, 90..250), PushResult::Stop));
        assert_eq!(tl.buffered(), vec![(0, 300 * PKT)]);
        assert_eq!(tl.range_at(0).unwrap().2, RangeState::Filling(b));
        for i in 0..300 {
            assert_eq!(ready(&tl, i * PKT), pkt(i));
        }
    }

    #[test]
    fn merged_ranges_read_back_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(spill_into(&dir, 1 << 30, 1 << 20));
        let reader = tl.add_reader(0);
        let a = tl.register_producer(Some(100 * PKT));
        fill(&tl, a, 100..400);
        let b = tl.register_producer(None);
        // stops once it reaches what `a` has
        assert!(matches!(fill(&tl, b, 0..120), PushResult::Stop));
        tl.finish(a, Finish::Eof);
        assert_eq!(tl.buffered(), vec![(0, 400 * PKT)]);

        // far away, so that every block is read back from its spill file
        tl.set_playhead(reader, 3600 * TICKS_PER_SECOND);
        assert_eq!(tl.stats().memory_bytes, 0);
        for i in 0..400 {
            assert_eq!(ready(&tl, i * PKT + PKT / 2), pkt(i));
        }
        assert!(matches!(tl.lookup(400 * PKT).unwrap(), Lookup::End));
    }

    #[test]
    fn long_media_keeps_a_small_index() {
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(spill_into(&dir, 1 << 30, 8 << 20));
        let p = tl.register_producer(None);
        let packets = packets_in(2 * 3600);
        for i in 0..packets {
            let pkt = Packet {
                data: Bytes::from_static(&[0; 8]),
                ..pkt(i)
            };
            tl.push(p, pkt);
        }
        tl.finish(p, Finish::Eof);

        let stats = tl.stats();
        assert_eq!(stats.packets, packets as usize);
        let blocks = tl.lock().ranges[0].blocks.len();
        assert_eq!(blocks, 2 * 3600, "one block per second");
        // the first minute stays in RAM, the rest only on disk
        assert!(
            stats.memory_bytes <= packets_in(61) as usize * 8,
            "{}",
            stats.memory_bytes
        );
        assert_eq!(
            ready(&tl, 5000 * TICKS_PER_SECOND).pts,
            5000 * TICKS_PER_SECOND
        );
    }

    #[test]
    fn redundant_producer_is_stopped() {
        let tl = Timeline::new(memory_only());
        let a = tl.register_producer(None);
        fill(&tl, a, 0..100);
        tl.finish(a, Finish::Eof);
        let b = tl.register_producer(Some(10 * PKT));
        assert!(matches!(tl.push(b, pkt(10)), PushResult::Stop));
    }

    #[test]
    fn a_block_a_cancel_seals_is_written_when_its_producer_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(spill_into(&dir, 1 << 30, 1 << 20));
        tl.add_reader(60 * TICKS_PER_SECOND);
        let p = tl.register_producer(None);
        fill(&tl, p, 0..3);
        // cancels come from wherever producers are dropped, so they only reserve the room
        tl.cancel(p);
        let reserved = tl.stats();
        assert!(reserved.disk_bytes > 0);
        assert_eq!(reserved.memory_bytes, 3 * 400, "held in RAM until written");

        tl.finish(p, Finish::Stopped);
        assert_eq!(
            tl.stats().memory_bytes,
            0,
            "written, and far behind the playhead"
        );
        assert_eq!(ready(&tl, 0), pkt(0));
    }

    #[test]
    fn spilled_payloads_leave_ram_outside_asymmetric_window() {
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(spill_into(&dir, 1 << 30, 1 << 20));
        let reader = tl.add_reader(0);
        let p = tl.register_producer(None);
        fill(&tl, p, 0..packets_in(300));
        // playhead 0: only the first ~60 s (15 s * bias 4) stays in RAM
        let in_ram = tl.stats().memory_bytes / 400;
        assert!(
            (packets_in(59)..=packets_in(61)).contains(&(in_ram as i64)),
            "{in_ram}"
        );

        tl.set_playhead(reader, 150 * TICKS_PER_SECOND);
        let in_ram = tl.stats().memory_bytes / 400;
        // 0..60 s is now 90 s behind and released; nothing new is pulled into RAM
        assert_eq!(in_ram, 0);

        // everything is still readable from disk
        for i in [0, packets_in(100), packets_in(299)] {
            assert_eq!(ready(&tl, i * PKT), pkt(i));
        }
    }

    /// Whether the block holding `t` has its packets in RAM.
    fn in_ram(tl: &Timeline, t: Ticks) -> bool {
        let g = tl.lock();
        let Probe::Buffered { range, block } = g.probe(t) else {
            panic!("{t} is not buffered");
        };
        g.ranges[range].blocks[block].mem.is_some()
    }

    #[test]
    fn retention_keeps_what_any_reader_is_near() {
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(spill_into(&dir, 1 << 30, 1 << 20));
        let ending = tl.add_reader(0);
        let p = tl.register_producer(None);
        fill(&tl, p, 0..packets_in(300));
        tl.finish(p, Finish::Eof);
        // each reader reads what it plays, from disk
        let play = |seconds: std::ops::Range<i64>| {
            for i in packets_in(seconds.start)..packets_in(seconds.end) {
                assert_eq!(ready(&tl, i * PKT), pkt(i));
            }
        };
        tl.set_playhead(ending, 290 * TICKS_PER_SECOND);
        play(290..291);
        assert!(!in_ram(&tl, 0));

        // the media again from the start, while the first reader plays on
        let replaying = tl.add_reader(0);
        play(0..2);
        tl.set_playhead(replaying, 2 * TICKS_PER_SECOND);
        assert!(in_ram(&tl, 0));
        assert!(in_ram(&tl, 290 * TICKS_PER_SECOND));

        // once the first reader is done, only what the other one is near stays
        tl.remove_reader(ending);
        tl.set_playhead(replaying, 4 * TICKS_PER_SECOND);
        assert!(in_ram(&tl, 0));
        assert!(!in_ram(&tl, 290 * TICKS_PER_SECOND));
    }

    #[test]
    fn complete_media_parks_on_disk_out_of_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Arc::new(CacheBudget::new(1 << 30, 1 << 30));
        let cfg = RetentionConfig {
            budget: Some(Arc::clone(&budget)),
            ..spill_into(&dir, 1 << 30, 1 << 20)
        };
        let tl = Timeline::new(cfg.clone());
        let _other = Timeline::new(cfg);
        let reader = tl.add_reader(0);
        let p = tl.register_producer(None);
        fill(&tl, p, 0..packets_in(30));
        assert_eq!(tl.complete_on_disk(), None, "still downloading");
        assert_eq!(tl.park(), None);
        tl.finish(p, Finish::Eof);
        let bytes = tl.complete_on_disk().expect("all of it is on disk");
        assert_eq!(bytes, tl.stats().disk_bytes);

        assert_eq!(tl.park(), Some(bytes));
        assert_eq!(
            budget.timelines.load(Ordering::Relaxed),
            1,
            "it left the budget"
        );
        assert!(in_ram(&tl, 0), "its reader still plays from RAM");
        tl.remove_reader(reader);
        assert_eq!(tl.stats().memory_bytes, 0, "nobody reads it");

        // it plays again from disk, and lets go of RAM once done
        let again = tl.add_reader(0);
        for i in 0..packets_in(30) {
            assert_eq!(ready(&tl, i * PKT), pkt(i));
        }
        tl.remove_reader(again);
        assert_eq!(tl.stats().memory_bytes, 0);
    }

    #[test]
    fn media_not_downloaded_from_its_start_is_not_complete() {
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(spill_into(&dir, 1 << 30, 1 << 20));
        let p = tl.register_producer(Some(10 * TICKS_PER_SECOND));
        fill(&tl, p, packets_in(10)..packets_in(30));
        tl.finish(p, Finish::Eof);
        assert_eq!(tl.complete_on_disk(), None, "the first 10 s are missing");

        let memory = Timeline::new(memory_only());
        let p = memory.register_producer(None);
        fill(&memory, p, 0..packets_in(5));
        memory.finish(p, Finish::Eof);
        assert_eq!(memory.park(), None, "without spill nothing is on disk");
    }

    #[test]
    fn ram_only_eviction_prefers_data_behind_the_playhead() {
        let tl = Timeline::new(RetentionConfig {
            memory_max_bytes: packets_in(100) as usize * 400,
            ..memory_only()
        });
        let reader = tl.add_reader(0);
        let p = tl.register_producer(None);
        fill(&tl, p, 0..packets_in(100));
        tl.set_playhead(reader, 50 * TICKS_PER_SECOND);
        // the cap is full; pushing another 10 s forces 10 s out
        fill(&tl, p, packets_in(100)..packets_in(110));
        let (start, end) = tl.buffered()[0];
        // 50 s behind vs 60 s ahead (= 15 s weighted): the oldest data goes first
        assert!(start >= 10 * TICKS_PER_SECOND, "start {start}");
        assert_eq!(end, 110 * TICKS_PER_SECOND);
    }

    #[test]
    fn timelines_sharing_a_budget_take_equal_shares() {
        let seconds = |s: i64| packets_in(s) as usize * 400;
        let cfg = RetentionConfig {
            budget: Some(Arc::new(CacheBudget::new(seconds(100), u64::MAX))),
            ..memory_only()
        };
        let tl = Timeline::new(cfg.clone());
        tl.add_reader(80 * TICKS_PER_SECOND);
        let p = tl.register_producer(None);
        // alone, it may take the whole budget
        fill(&tl, p, 0..packets_in(80));
        assert_eq!(tl.stats().memory_bytes, seconds(80));

        // a second timeline halves its share, which it keeps to from its next packet on
        let other = Timeline::new(cfg);
        fill(&tl, p, packets_in(80)..packets_in(81));
        assert!(tl.stats().memory_bytes <= seconds(50));
        let (start, end) = tl.buffered()[0];
        assert!(
            start >= 31 * TICKS_PER_SECOND,
            "the oldest data went first: {start}"
        );
        assert_eq!(end, 81 * TICKS_PER_SECOND);

        // once the other one is gone, the whole budget is the first one's again
        drop(other);
        fill(&tl, p, packets_in(81)..packets_in(120));
        assert_eq!(tl.buffered()[0], (start, 120 * TICKS_PER_SECOND));
    }

    #[test]
    fn backpressure_pauses_producer_until_playhead_moves() {
        let dir = tempfile::tempdir().unwrap();
        let seg = packets_in(10) as u64 * 400; // ~10 s per spill file
        let tl = Arc::new(Timeline::new(spill_into(&dir, seg * 6, seg)));
        let reader = tl.add_reader(0);
        let p = tl.register_producer(None);
        let worker = std::thread::spawn({
            let tl = tl.clone();
            move || fill(&tl, p, 0..packets_in(600))
        });

        std::thread::sleep(Duration::from_millis(200));
        let (_, end) = tl.buffered()[0];
        // ~60 s fits on disk and nothing is behind the playhead yet, so the producer waits
        assert!(end <= 61 * TICKS_PER_SECOND, "{end}");
        assert!(!worker.is_finished());

        // move the playhead; data behind it becomes evictable and the producer resumes
        tl.set_playhead(reader, 40 * TICKS_PER_SECOND);
        std::thread::sleep(Duration::from_millis(200));
        let (start, end2) = tl.buffered()[0];
        assert!(end2 > end, "producer did not resume");
        assert!(start > 0, "nothing behind the playhead was evicted");
        // with bias 4, 40 s behind would outweigh up to 160 s ahead; disk only holds 60 s
        assert!(start <= 40 * TICKS_PER_SECOND);

        tl.cancel(p);
        assert!(matches!(worker.join().unwrap(), PushResult::Stop));
        assert!(tl.stats().disk_bytes <= seg * 6);
    }

    #[test]
    fn spilled_blocks_round_trip_and_corruption_is_an_error() {
        let packets: Vec<Packet> = (0..5)
            .map(|i| Packet {
                trim_start: 312,
                trim_end: i as u32,
                ..pkt(i)
            })
            .collect();
        let encoded = encode_block(&packets);
        assert_eq!(decode_block(Bytes::from(encoded.clone())).unwrap(), packets);

        let truncated = Bytes::copy_from_slice(&encoded[..encoded.len() - 1]);
        assert_eq!(
            decode_block(truncated).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(decode_block(Bytes::new()).is_err());
    }
}
