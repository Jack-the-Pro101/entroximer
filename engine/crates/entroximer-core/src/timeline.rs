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
//! Storage is tiered for RAM-constrained hosts:
//! - the index (~40 bytes per packet) always lives in RAM,
//! - payloads are written through to anonymous temp files ("spill", on by default),
//! - payloads are additionally kept in RAM only inside a window around the playhead that reaches
//!   `forward_bias` times further ahead than behind, since listeners skip forward far more often
//!   than they skip back.
//!
//! When a cap is hit, the unit with the largest *weighted distance* from the playhead is evicted,
//! where distance ahead counts `1 / forward_bias` as much as distance behind. If the incoming
//! packet would itself be the farthest unit, the producer is paused (backpressure) instead, so a
//! source that cannot seek by time never has to be restarted from the beginning.

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use symphonia::core::codecs::audio::AudioCodecParameters;

use crate::error::{CodecMismatch, CodecSummary, MediaError};

/// Media time in 48 kHz sample units (Discord's output rate).
pub type Ticks = i64;
pub const TICKS_PER_SECOND: Ticks = 48_000;

pub type ProducerId = u64;

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

/// A producer's packet that lands this far past its own range's end starts a new range.
const MAX_HOLE: Ticks = TICKS_PER_SECOND / 10;

/// How often (in playhead movement) payloads outside the RAM window are released.
const SWEEP_INTERVAL: Ticks = TICKS_PER_SECOND;

#[derive(Clone, Debug, PartialEq)]
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
    /// Hard cap on payload bytes held in RAM per timeline.
    pub memory_max_bytes: usize,
    /// Disk spill; `None` keeps every payload in RAM (bounded by `memory_max_bytes`).
    pub spill: Option<SpillConfig>,
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
        }
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

#[derive(Clone, Copy, Debug)]
struct DiskLoc {
    segment: u64,
    offset: u64,
}

/// Index entry for one packet; `len` is a `u32` to keep the always-resident index small.
struct Entry {
    pts: Ticks,
    dur: Ticks,
    trim_start: u32,
    trim_end: u32,
    len: u32,
    mem: Option<Bytes>,
    disk: Option<DiskLoc>,
}

impl Entry {
    fn end(&self) -> Ticks {
        self.pts + self.dur
    }
}

/// Never empty; ranges are kept sorted by start and do not overlap (beyond `tolerance`).
struct Range {
    entries: VecDeque<Entry>,
    state: RangeState,
}

impl Range {
    fn start(&self) -> Ticks {
        self.entries.front().map_or(0, |e| e.pts)
    }

    fn end(&self) -> Ticks {
        self.entries.back().map_or(0, Entry::end)
    }

    /// Index of the first entry that ends after `t`.
    fn index_at(&self, t: Ticks) -> usize {
        self.entries.partition_point(|e| e.end() <= t)
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
    playhead: Ticks,
    last_sweep: Ticks,
    media_end: Option<Ticks>,
    failure: Option<MediaError>,
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
    Buffered { range: usize, entry: usize },
    Pending(Pending),
    End,
}

enum Payload {
    Memory(Bytes),
    Disk(Arc<File>, u64),
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
                playhead: 0,
                last_sweep: 0,
                media_end: None,
                failure: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
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
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let result = g.insert(producer, pkt);
        g.enforce_caps();
        drop(g);
        self.changed.notify_all();
        result
    }

    pub fn finish(&self, producer: ProducerId, outcome: Finish) {
        let mut g = self.lock();
        let Some(p) = g.producers.remove(&producer) else {
            return;
        };
        // a cancelled producer's stream ends early; that is not the end of the media
        let outcome = match outcome {
            Finish::Eof if p.cancelled => Finish::Stopped,
            other => other,
        };
        let range = g
            .ranges
            .iter_mut()
            .find(|r| r.state == RangeState::Filling(producer));
        match outcome {
            Finish::Eof => {
                let end = match range {
                    Some(r) => {
                        r.state = RangeState::Complete;
                        r.end()
                    }
                    // nothing delivered: the producer was opened at or past the end
                    None => p.requested_start,
                };
                g.media_end = Some(g.media_end.map_or(end, |e| e.min(end)));
            }
            Finish::Stopped | Finish::Failed(_) => {
                if let Some(r) = range {
                    r.state = RangeState::Truncated;
                }
            }
        }
        if let Finish::Failed(msg) = outcome {
            g.failure = Some(msg);
        }
        drop(g);
        self.changed.notify_all();
    }

    /// Stops accepting packets from `producer`; its range becomes `Truncated`.
    pub fn cancel(&self, producer: ProducerId) {
        let mut g = self.lock();
        if let Some(p) = g.producers.get_mut(&producer) {
            p.cancelled = true;
        }
        if let Some(r) = g
            .ranges
            .iter_mut()
            .find(|r| r.state == RangeState::Filling(producer))
        {
            r.state = RangeState::Truncated;
        }
        drop(g);
        self.changed.notify_all();
    }

    /// The packet covering `t`, or why there is none. Reads spilled payloads from disk.
    pub fn lookup(&self, t: Ticks) -> io::Result<Lookup> {
        let g = self.lock();
        let (range, entry) = match g.probe(t) {
            Probe::Buffered { range, entry } => (range, entry),
            Probe::Pending(pending) => return Ok(Lookup::Pending(pending)),
            Probe::End => return Ok(Lookup::End),
        };
        let e = &g.ranges[range].entries[entry];
        let payload = match (&e.mem, e.disk) {
            (Some(bytes), _) => Payload::Memory(bytes.clone()),
            (None, Some(loc)) => {
                let segment = g
                    .segments
                    .iter()
                    .find(|s| s.id == loc.segment)
                    .expect("entries never point at evicted segments");
                Payload::Disk(segment.file.clone(), loc.offset)
            }
            (None, None) => unreachable!("entries without a payload are removed"),
        };
        let (pts, dur, trim_start, trim_end, len) = (e.pts, e.dur, e.trim_start, e.trim_end, e.len);
        drop(g);

        let data = match payload {
            Payload::Memory(bytes) => bytes,
            Payload::Disk(file, offset) => {
                let mut buf = vec![0; len as usize];
                file.read_exact_at(&mut buf, offset)?;
                Bytes::from(buf)
            }
        };
        Ok(Lookup::Ready(Packet {
            pts,
            dur,
            trim_start,
            trim_end,
            data,
        }))
    }

    /// What is missing at `t`; `None` if `t` is buffered or past the end. Never touches disk.
    pub fn pending_at(&self, t: Ticks) -> Option<Pending> {
        match self.lock().probe(t) {
            Probe::Pending(pending) => Some(pending),
            Probe::Buffered { .. } | Probe::End => None,
        }
    }

    /// Informs retention of the current playback position; wakes paused producers.
    pub fn set_playhead(&self, t: Ticks) {
        let mut g = self.lock();
        g.playhead = t;
        if (t - g.last_sweep).abs() >= SWEEP_INTERVAL {
            g.last_sweep = t;
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

    /// The most recent producer failure, if any, which is cleared by this call.
    pub fn take_failure(&self) -> Option<MediaError> {
        self.lock().failure.take()
    }

    pub fn stats(&self) -> TimelineStats {
        let g = self.lock();
        TimelineStats {
            ranges: g.ranges.len(),
            packets: g.ranges.iter().map(|r| r.entries.len()).sum(),
            memory_bytes: g.memory_bytes,
            disk_bytes: g.disk_bytes,
        }
    }
}

impl Inner {
    fn probe(&self, t: Ticks) -> Probe {
        if self.media_end.is_some_and(|end| t >= end) {
            return Probe::End;
        }

        let mut frontier = None;
        let preceding = self.ranges.partition_point(|r| r.start() <= t + MAX_HOLE);
        if let Some(range) = preceding.checked_sub(1) {
            let r = &self.ranges[range];
            let entry = r.index_at(t);
            if entry < r.entries.len() {
                return Probe::Buffered { range, entry };
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

    fn spill_active(&self) -> bool {
        self.cfg.spill.is_some() && !self.spill_failed
    }

    fn weight(&self, start: Ticks, end: Ticks) -> Ticks {
        weighted_distance(self.playhead, self.cfg.forward_bias, start, end)
    }

    fn in_memory_window(&self, start: Ticks, end: Ticks) -> bool {
        self.weight(start, end) <= ticks(self.cfg.memory_behind)
    }

    fn over_cap(&self, extra: u64) -> bool {
        match &self.cfg.spill {
            Some(spill) if !self.spill_failed => self.disk_bytes + extra > spill.max_bytes,
            _ => self.memory_bytes as u64 + extra > self.cfg.memory_max_bytes as u64,
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
                .flat_map(|r| [r.entries.front(), r.entries.back()])
                .flatten()
                .map(|e| self.weight(e.pts, e.end()))
                .max()
        };
        w.filter(|&w| w > 0)
    }

    fn must_wait_for_space(&self, pkt: &Packet) -> bool {
        if !self.over_cap(pkt.data.len() as u64) {
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

    fn insert(&mut self, producer: ProducerId, pkt: Packet) -> PushResult {
        let p = self
            .producers
            .get_mut(&producer)
            .expect("checked by caller");
        p.first.get_or_insert((Instant::now(), pkt.pts));
        p.frontier = p.frontier.max(pkt.end());
        let tol = tolerance(pkt.dur);

        let own = self
            .ranges
            .iter()
            .position(|r| r.state == RangeState::Filling(producer));
        let idx = match own {
            Some(i) if pkt.pts < self.ranges[i].end() - tol => return PushResult::Continue,
            Some(i) if pkt.pts > self.ranges[i].end() + MAX_HOLE => {
                // our tail was evicted (or the stream jumped): continue in a new range
                self.ranges[i].state = RangeState::Truncated;
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

        let entry = self.make_entry(producer, pkt);
        self.ranges[idx].entries.push_back(entry);
        self.try_merge(idx, tol)
    }

    fn insert_new_range(&mut self, producer: ProducerId, pkt: Packet) -> PushResult {
        let tol = tolerance(pkt.dur);
        let start = pkt.pts;
        let entry = self.make_entry(producer, pkt);
        let idx = self.ranges.partition_point(|r| r.start() < start);
        self.ranges.insert(
            idx,
            Range {
                entries: VecDeque::from([entry]),
                state: RangeState::Filling(producer),
            },
        );
        self.try_merge(idx, tol)
    }

    /// Joins range `idx` with its successor once they touch; the producer filling `idx` stops.
    fn try_merge(&mut self, idx: usize, tol: Ticks) -> PushResult {
        if idx + 1 >= self.ranges.len()
            || self.ranges[idx].end() + tol < self.ranges[idx + 1].start()
        {
            return PushResult::Continue;
        }
        let next = self.ranges.remove(idx + 1);
        let end = self.ranges[idx].end();
        for e in next.entries {
            if e.pts >= end - tolerance(e.dur) {
                self.ranges[idx].entries.push_back(e);
            } else if let Some(m) = &e.mem {
                self.memory_bytes -= m.len();
            }
        }
        self.ranges[idx].state = next.state;
        PushResult::Stop
    }

    fn make_entry(&mut self, producer: ProducerId, pkt: Packet) -> Entry {
        let disk = self.write_disk(producer, &pkt);
        let keep = disk.is_none() || self.in_memory_window(pkt.pts, pkt.end());
        if keep {
            self.memory_bytes += pkt.data.len();
        }
        Entry {
            pts: pkt.pts,
            dur: pkt.dur,
            trim_start: pkt.trim_start,
            trim_end: pkt.trim_end,
            len: u32::try_from(pkt.data.len()).expect("packets are far smaller than 4 GiB"),
            mem: keep.then_some(pkt.data),
            disk,
        }
    }

    fn write_disk(&mut self, producer: ProducerId, pkt: &Packet) -> Option<DiskLoc> {
        if !self.spill_active() {
            return None;
        }
        let spill = self.cfg.spill.as_ref().expect("spill is active");
        let len = pkt.data.len() as u64;
        let current = self.producers[&producer]
            .segment
            .and_then(|id| self.segments.iter().position(|s| s.id == id))
            .filter(|&i| self.segments[i].len + len <= spill.segment_bytes);
        let si = match current {
            Some(i) => i,
            None => match tempfile::tempfile_in(&spill.dir) {
                Ok(file) => {
                    let id = self.next_segment;
                    self.next_segment += 1;
                    self.segments.push(Segment {
                        id,
                        file: Arc::new(file),
                        len: 0,
                        start: pkt.pts,
                        end: pkt.end(),
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
        if let Err(e) = seg.file.write_all_at(&pkt.data, seg.len) {
            tracing::warn!("disabling disk spill: {e}");
            self.spill_failed = true;
            return None;
        }
        let loc = DiskLoc {
            segment: seg.id,
            offset: seg.len,
        };
        seg.len += len;
        seg.start = seg.start.min(pkt.pts);
        seg.end = seg.end.max(pkt.end());
        self.disk_bytes += len;
        Some(loc)
    }

    /// Releases RAM copies of payloads that are safely on disk and outside the RAM window.
    fn sweep_memory(&mut self) {
        if !self.spill_active() {
            return;
        }
        let behind = ticks(self.cfg.memory_behind);
        let (playhead, bias) = (self.playhead, self.cfg.forward_bias);
        let weight = |e: &Entry| weighted_distance(playhead, bias, e.pts, e.end());
        let mut freed = 0;
        for e in self.ranges.iter_mut().flat_map(|r| r.entries.iter_mut()) {
            if e.disk.is_some()
                && weight(e) > behind
                && let Some(m) = e.mem.take()
            {
                freed += m.len();
            }
        }
        self.memory_bytes -= freed;

        if self.memory_bytes > self.cfg.memory_max_bytes {
            let mut held: Vec<(Ticks, usize, usize)> = Vec::new();
            for (ri, r) in self.ranges.iter().enumerate() {
                for (ei, e) in r.entries.iter().enumerate() {
                    if e.mem.is_some() && e.disk.is_some() {
                        held.push((weight(e), ri, ei));
                    }
                }
            }
            held.sort_unstable_by_key(|&(weight, ..)| Reverse(weight));
            for (_, ri, ei) in held {
                if self.memory_bytes <= self.cfg.memory_max_bytes {
                    break;
                }
                if let Some(m) = self.ranges[ri].entries[ei].mem.take() {
                    self.memory_bytes -= m.len();
                }
            }
        }
    }

    fn enforce_caps(&mut self) {
        if self.spill_active() {
            let max = self.cfg.spill.as_ref().map_or(u64::MAX, |s| s.max_bytes);
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
            if self.memory_bytes > self.cfg.memory_max_bytes {
                self.sweep_memory();
            }
        } else {
            while self.memory_bytes > self.cfg.memory_max_bytes {
                if !self.evict_farthest_endpoint() {
                    break;
                }
            }
        }
    }

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
        for e in self.ranges.iter_mut().flat_map(|r| r.entries.iter_mut()) {
            if e.disk.is_some_and(|d| d.segment == id) {
                e.disk = None;
            }
        }
        self.retain_entries(|e| e.mem.is_some() || e.disk.is_some());
    }

    /// Drops up to ~1 s from whichever range end is farthest from the playhead.
    fn evict_farthest_endpoint(&mut self) -> bool {
        let (playhead, bias) = (self.playhead, self.cfg.forward_bias);
        let farthest = self
            .ranges
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                let front = r.entries.front().map(|e| (e, RangeEnd::Front));
                let back = r.entries.back().map(|e| (e, RangeEnd::Back));
                front.into_iter().chain(back).map(move |(e, end)| {
                    (weighted_distance(playhead, bias, e.pts, e.end()), i, end)
                })
            })
            .filter(|&(weight, ..)| weight > 0)
            .max_by_key(|&(weight, ..)| weight);
        let Some((_, i, end)) = farthest else {
            return false;
        };

        let range = &mut self.ranges[i];
        let (mut dropped, mut freed) = (0, 0);
        while dropped < TICKS_PER_SECOND {
            let entry = match end {
                RangeEnd::Front => range.entries.pop_front(),
                RangeEnd::Back => range.entries.pop_back(),
            };
            let Some(entry) = entry else { break };
            dropped += entry.dur;
            freed += entry.mem.map_or(0, |m| m.len());
        }
        self.memory_bytes -= freed;
        if range.entries.is_empty() {
            self.ranges.remove(i);
        } else if end == RangeEnd::Back && range.state == RangeState::Complete {
            // the end of the media is no longer buffered
            range.state = RangeState::Truncated;
        }
        true
    }

    /// Removes entries failing `keep`, splitting ranges around the holes. Only the run that still
    /// reaches the original end keeps the range's state; every other run is `Truncated`.
    fn retain_entries(&mut self, keep: impl Fn(&Entry) -> bool) {
        let mut out = Vec::with_capacity(self.ranges.len());
        for r in std::mem::take(&mut self.ranges) {
            let mut runs: Vec<VecDeque<Entry>> = Vec::new();
            let mut cur = VecDeque::new();
            for e in r.entries {
                if keep(&e) {
                    cur.push_back(e);
                } else {
                    if let Some(m) = &e.mem {
                        self.memory_bytes -= m.len();
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
            for (k, entries) in runs.into_iter().enumerate() {
                let state = if k == last && tail_kept {
                    r.state
                } else {
                    RangeState::Truncated
                };
                out.push(Range { entries, state });
            }
        }
        self.ranges = out;
    }
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
    fn redundant_producer_is_stopped() {
        let tl = Timeline::new(memory_only());
        let a = tl.register_producer(None);
        fill(&tl, a, 0..100);
        tl.finish(a, Finish::Eof);
        let b = tl.register_producer(Some(10 * PKT));
        assert!(matches!(tl.push(b, pkt(10)), PushResult::Stop));
    }

    #[test]
    fn spilled_payloads_leave_ram_outside_asymmetric_window() {
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(spill_into(&dir, 1 << 30, 1 << 20));
        let p = tl.register_producer(None);
        fill(&tl, p, 0..packets_in(300));
        // playhead 0: only the first ~60 s (15 s * bias 4) stays in RAM
        let in_ram = tl.stats().memory_bytes / 400;
        assert!(
            (packets_in(59)..=packets_in(61)).contains(&(in_ram as i64)),
            "{in_ram}"
        );

        tl.set_playhead(150 * TICKS_PER_SECOND);
        let in_ram = tl.stats().memory_bytes / 400;
        // 0..60 s is now 90 s behind and released; nothing new is pulled into RAM
        assert_eq!(in_ram, 0);

        // everything is still readable from disk
        for i in [0, packets_in(100), packets_in(299)] {
            assert_eq!(ready(&tl, i * PKT), pkt(i));
        }
    }

    #[test]
    fn ram_only_eviction_prefers_data_behind_the_playhead() {
        let tl = Timeline::new(RetentionConfig {
            memory_max_bytes: packets_in(100) as usize * 400,
            ..memory_only()
        });
        let p = tl.register_producer(None);
        fill(&tl, p, 0..packets_in(100));
        tl.set_playhead(50 * TICKS_PER_SECOND);
        // the cap is full; pushing another 10 s forces 10 s out
        fill(&tl, p, packets_in(100)..packets_in(110));
        let (start, end) = tl.buffered()[0];
        // 50 s behind vs 60 s ahead (= 15 s weighted): the oldest data goes first
        assert!(start >= 10 * TICKS_PER_SECOND, "start {start}");
        assert_eq!(end, 110 * TICKS_PER_SECOND);
    }

    #[test]
    fn backpressure_pauses_producer_until_playhead_moves() {
        let dir = tempfile::tempdir().unwrap();
        let seg = packets_in(10) as u64 * 400; // 10 s per spill file
        let tl = Arc::new(Timeline::new(spill_into(&dir, seg * 6, seg)));
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
        tl.set_playhead(40 * TICKS_PER_SECOND);
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
}
