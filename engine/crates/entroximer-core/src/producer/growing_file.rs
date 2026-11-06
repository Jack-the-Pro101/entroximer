//! Streams media that another process is still writing to files.
//!
//! yt-dlp's SABR downloader (yt-dlp PR #13515) cannot write to stdout, so it is pointed at an
//! empty per-open directory and whatever media appears there is read while it grows, until the
//! process exits. Only files named after the media (`<stem>.…`) count: the SABR downloader also
//! saves its progress there through a temporary file (`tmpritx0d36`) renamed to `<name>.state`.
//!
//! - a plain download writes one `<name>.part`, renamed to `<name>` when done; the open file
//!   descriptor survives the rename;
//! - the SABR downloader writes its initialization segment to `<name>.sqi.part` and each run of
//!   consecutive segments to `<name>.sq<first segment>.part`, a whole segment at a time, then
//!   joins them in that order into `<name>`. The init file and the first run are read as they
//!   grow; what follows a gap waits for `<name>`.
//!
//! Moving on to the finished file, its start is checked against what was already streamed, so
//! a layout misjudged here costs waiting for the download, never corrupt media.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use tokio::io::AsyncReadExt;

use super::process::{ProcessSpec, RunningProcess};
use super::{Producer, ProducerCaps, ProducerStream, READ_CHUNK, TimestampMode};
use crate::timeline::Ticks;

/// Files the writer may create next to the media that must not be mistaken for it.
const IGNORED_SUFFIXES: &[&str] = &[".json", ".ytdl", ".state", ".txt", ".log"];

pub struct GrowingFileProducer<F> {
    build: F,
    work_dir: PathBuf,
    stem: String,
    caps: ProducerCaps,
    hint: Option<String>,
    pub poll: Duration,
}

impl<F> GrowingFileProducer<F>
where
    F: Fn(Option<Ticks>, &Path) -> (ProcessSpec, TimestampMode) + Send + Sync + 'static,
{
    /// `build` receives the requested start and an empty directory to write into, which is
    /// created under `work_dir` and removed when the stream is dropped. The process must name
    /// the media `<stem>.<ext>` there; other files are left alone.
    pub fn new(
        work_dir: PathBuf,
        stem: impl Into<String>,
        caps: ProducerCaps,
        hint: Option<String>,
        build: F,
    ) -> Self {
        Self {
            build,
            work_dir,
            stem: stem.into(),
            caps,
            hint,
            poll: Duration::from_millis(50),
        }
    }
}

impl<F> std::fmt::Debug for GrowingFileProducer<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrowingFileProducer")
            .field("work_dir", &self.work_dir)
            .field("stem", &self.stem)
            .field("caps", &self.caps)
            .field("hint", &self.hint)
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl<F> Producer for GrowingFileProducer<F>
where
    F: Fn(Option<Ticks>, &Path) -> (ProcessSpec, TimestampMode) + Send + Sync + 'static,
{
    fn caps(&self) -> ProducerCaps {
        self.caps
    }

    fn open(&self, start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        let opened = tempfile::tempdir_in(&self.work_dir).and_then(|dir| {
            let (spec, timestamps) = (self.build)(start, dir.path());
            Ok((dir, spec.spawn(Stdio::null())?, timestamps))
        });
        let (stem, hint, poll) = (self.stem.clone(), self.hint.clone(), self.poll);
        Box::pin(async move {
            let (dir, proc, timestamps) = opened?;
            let tail = Tail {
                dir,
                stem,
                proc,
                reading: None,
                sent: 0,
                hash: FNV_OFFSET,
                poll,
                done: false,
            };
            Ok(ProducerStream {
                bytes: futures_util::stream::unfold(tail, |mut t| async move {
                    let item = t.next_chunk().await?;
                    Some((item, t))
                })
                .boxed(),
                timestamps,
                hint,
            })
        })
    }
}

struct Tail {
    dir: tempfile::TempDir,
    /// What the media's file names start with, before the first dot.
    stem: String,
    proc: RunningProcess,
    /// The piece being read.
    reading: Option<(Piece, tokio::fs::File)>,
    /// Bytes streamed so far, and their hash, to check that the finished file continues them.
    sent: u64,
    hash: u64,
    poll: Duration,
    done: bool,
}

impl Tail {
    async fn next_chunk(&mut self) -> Option<io::Result<Bytes>> {
        let mut buf = vec![0; READ_CHUNK];
        loop {
            if self.done {
                return None;
            }
            // before reading: once the writer is gone, reading nothing means everything is read
            let exited = match self.proc.child.try_wait() {
                Ok(status) => status.is_some(),
                Err(e) => return self.fail(e),
            };
            if let Some((_, file)) = &mut self.reading {
                match file.read(&mut buf).await {
                    Ok(0) => {}
                    Ok(n) => {
                        buf.truncate(n);
                        self.sent += n as u64;
                        self.hash = fnv1a(self.hash, &buf);
                        return Some(Ok(Bytes::from(buf)));
                    }
                    Err(e) => return self.fail(e),
                }
            }
            let pieces = match Pieces::list(self.dir.path(), &self.stem).await {
                Ok(pieces) => pieces,
                Err(e) => return self.fail(e),
            };
            let current = self.reading.as_ref().map(|(piece, _)| *piece);
            match next_step(current, &pieces, exited) {
                Step::Wait => tokio::time::sleep(self.poll).await,
                Step::Read(piece, path) => match self.open(piece, &path).await {
                    Ok(Some(file)) => self.reading = Some((piece, file)),
                    // renamed or joined between listing and opening: look again
                    Ok(None) => {}
                    Err(e) => return self.fail(e),
                },
                Step::End => {
                    self.done = true;
                    return self.proc.finish().await.err().map(Err);
                }
                Step::Incomplete => {
                    self.done = true;
                    let err = match self.proc.finish().await {
                        Ok(()) if current.is_none() => {
                            io::Error::other("producer exited without writing media")
                        }
                        Ok(()) => io::Error::other("producer exited before finishing the media"),
                        Err(e) => e,
                    };
                    return Some(Err(err));
                }
            }
        }
    }

    /// Opens `piece`; the finished file is positioned after what was streamed, once its start
    /// is found to match. `None` if it is gone.
    async fn open(&self, piece: Piece, path: &Path) -> io::Result<Option<tokio::fs::File>> {
        let mut file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if piece == Piece::Finished && self.sent > 0 {
            let mut hash = FNV_OFFSET;
            let mut left = self.sent;
            let mut buf = vec![0; READ_CHUNK];
            while left > 0 {
                let want = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
                let n = file.read(&mut buf[..want]).await?;
                if n == 0 {
                    break;
                }
                hash = fnv1a(hash, &buf[..n]);
                left -= n as u64;
            }
            if left > 0 || hash != self.hash {
                return Err(io::Error::other(
                    "the finished file does not continue the media streamed from its pieces",
                ));
            }
        }
        Ok(Some(file))
    }

    fn fail(&mut self, e: io::Error) -> Option<io::Result<Bytes>> {
        self.done = true;
        Some(Err(e))
    }
}

/// What a file in the download directory holds, judged by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
    /// SABR's initialization segment: `<name>.sqi.part`.
    Init,
    /// A SABR run of consecutive segments: `<name>.sq<first segment>.part`.
    Run(u64),
    /// Being written: the whole media of a plain download, or SABR joining its pieces.
    Partial,
    /// The whole media, finished.
    Finished,
}

impl Piece {
    /// `None` for files that are not the media named `<stem>.…`, or not yet part of it.
    fn of(name: &str, stem: &str) -> Option<Self> {
        let named_after_media = name
            .strip_prefix(stem)
            .is_some_and(|rest| rest.starts_with('.'));
        if !named_after_media || IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s)) {
            return None;
        }
        let Some(stem) = name.strip_suffix(".part") else {
            return Some(Self::Finished);
        };
        let suffix = stem.rsplit_once('.').map_or("", |(_, suffix)| suffix);
        if suffix == "sqi" {
            Some(Self::Init)
        } else if let Some(first) = suffix.strip_prefix("sq").and_then(|n| n.parse().ok()) {
            Some(Self::Run(first))
        } else if suffix.strip_prefix("sg").is_some_and(|id| !id.is_empty()) {
            // a SABR segment still arriving, appended to its run once complete
            None
        } else {
            Some(Self::Partial)
        }
    }
}

/// The media files in the download directory.
#[derive(Debug, Default)]
struct Pieces {
    init: Option<PathBuf>,
    /// By first segment, with their current sizes.
    runs: BTreeMap<u64, (PathBuf, u64)>,
    partial: Option<PathBuf>,
    finished: Option<PathBuf>,
}

impl Pieces {
    async fn list(dir: &Path, stem: &str) -> io::Result<Self> {
        let mut pieces = Self::default();
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let Some(piece) = Piece::of(&entry.file_name().to_string_lossy(), stem) else {
                continue;
            };
            // gone since listing: the next listing tells
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let path = entry.path();
            let first = |slot: &mut Option<PathBuf>| match slot {
                Some(existing) if *existing <= path => {}
                _ => *slot = Some(path.clone()),
            };
            match piece {
                Piece::Init => first(&mut pieces.init),
                Piece::Run(n) => {
                    pieces.runs.insert(n, (path.clone(), metadata.len()));
                }
                Piece::Partial => first(&mut pieces.partial),
                Piece::Finished => first(&mut pieces.finished),
            }
        }
        Ok(pieces)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Read(Piece, PathBuf),
    Wait,
    /// Everything there is has been read.
    End,
    /// The writer is gone without leaving the rest of the media.
    Incomplete,
}

/// What to read once the piece being read (if any) has nothing more for now.
fn next_step(current: Option<Piece>, pieces: &Pieces, exited: bool) -> Step {
    let finished = || {
        pieces
            .finished
            .clone()
            .map(|path| Step::Read(Piece::Finished, path))
    };
    let first_run = || pieces.runs.iter().next();
    let waiting = || if exited { Step::Incomplete } else { Step::Wait };
    match current {
        // read to the end through the open descriptor, whatever the file is renamed to
        Some(Piece::Partial | Piece::Finished) => {
            if exited {
                Step::End
            } else {
                Step::Wait
            }
        }
        // a run follows once its first segment is complete, which ends the init segment too
        Some(Piece::Init) => match first_run() {
            Some((&n, (path, len))) if *len > 0 => Step::Read(Piece::Run(n), path.clone()),
            _ => finished().unwrap_or_else(waiting),
        },
        // a later run means a gap; only the joined file has what follows
        Some(Piece::Run(_)) => finished().unwrap_or_else(waiting),
        None => {
            if let Some(init) = &pieces.init {
                Step::Read(Piece::Init, init.clone())
            } else if let Some((&n, (path, _))) = first_run().filter(|(_, (_, len))| *len > 0) {
                // live streams have no initialization segment
                Step::Read(Piece::Run(n), path.clone())
            } else if let Some(step) = finished() {
                step
            } else if let Some(partial) = &pieces.partial {
                Step::Read(Piece::Partial, partial.clone())
            } else {
                waiting()
            }
        }
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, continued over `bytes`: detects a finished file that differs from what was streamed.
fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tails_file_while_writer_runs() {
        let work = tempfile::tempdir().unwrap();
        // writes three chunks with pauses, via a .part file renamed at the end like yt-dlp does
        let producer = GrowingFileProducer::new(
            work.path().to_owned(),
            "x",
            ProducerCaps::default(),
            None,
            |_, dir: &Path| {
                let script = format!(
                    "cd '{}' && printf aaa > x.part && sleep 0.2 && printf bbb >> x.part \
                     && sleep 0.2 && printf ccc >> x.part && mv x.part x.webm",
                    dir.display()
                );
                (
                    ProcessSpec::new("sh").arg("-c").arg(script),
                    TimestampMode::Absolute,
                )
            },
        );
        let stream = producer.open(None).await.unwrap();
        let chunks: Vec<_> = stream.bytes.collect().await;
        let data: Vec<u8> = chunks
            .into_iter()
            .flat_map(|c| c.unwrap().to_vec())
            .collect();
        assert_eq!(data, b"aaabbbccc");
    }

    /// Streams what `script` writes in the download directory.
    async fn open_script(
        work: &Path,
        script: impl Into<String>,
    ) -> futures_util::stream::BoxStream<'static, io::Result<Bytes>> {
        let script = script.into();
        let producer = GrowingFileProducer::new(
            work.to_owned(),
            "x",
            ProducerCaps::default(),
            None,
            move |_, dir: &Path| {
                (
                    ProcessSpec::new("sh")
                        .arg("-c")
                        .arg(format!("cd '{}' && {script}", dir.display())),
                    TimestampMode::Absolute,
                )
            },
        );
        producer.open(None).await.unwrap().bytes
    }

    async fn stream_of(script: impl Into<String>) -> Vec<io::Result<Bytes>> {
        let work = tempfile::tempdir().unwrap();
        open_script(work.path(), script).await.collect().await
    }

    fn joined(chunks: Vec<io::Result<Bytes>>) -> Vec<u8> {
        chunks
            .into_iter()
            .flat_map(|c| c.unwrap().to_vec())
            .collect()
    }

    /// Joins the pieces like yt-dlp's SABR downloader: init, then runs by first segment.
    const SABR_JOIN: &str = "sleep 0.2 && cat x.webm.sqi.part $(ls x.webm.sq[0-9]*.part | sort -t q -k 2 -n) \
         > x.webm.part && mv x.webm.part x.webm && rm -f x.webm.sq*.part x.webm.state";

    #[tokio::test]
    async fn streams_sabr_pieces_while_they_download() {
        let started = std::time::Instant::now();
        let work = tempfile::tempdir().unwrap();
        let mut bytes = open_script(
            work.path(),
            format!(
                "printf II > x.webm.sgi.part && mv x.webm.sgi.part x.webm.sqi.part \
                 && printf state > x.webm.state && printf aa > x.webm.sg1.part \
                 && printf aaa > x.webm.sq1.part && rm x.webm.sg1.part && sleep 0.2 \
                 && printf bbb >> x.webm.sq1.part && sleep 1 && {SABR_JOIN}"
            ),
        )
        .await;
        let first = bytes.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"II");
        let mut data = first.to_vec();
        while let Some(chunk) = bytes.next().await {
            data.extend_from_slice(&chunk.unwrap());
            if data == b"IIaaabbb" {
                // streamed from the pieces, well before the join after a second
                assert!(started.elapsed() < Duration::from_millis(900));
            }
        }
        assert_eq!(data, b"IIaaabbb");
    }

    #[tokio::test]
    async fn sabr_gaps_wait_for_the_joined_file() {
        let chunks = stream_of(format!(
            "printf II > x.webm.sqi.part && printf aaa > x.webm.sq1.part \
             && printf eee > x.webm.sq5.part && {SABR_JOIN}"
        ))
        .await;
        assert_eq!(joined(chunks), b"IIaaaeee");
    }

    #[tokio::test]
    async fn a_joined_file_that_differs_is_an_error() {
        let chunks = stream_of(
            "printf II > x.webm.sqi.part && printf aaa > x.webm.sq1.part \
             && sleep 0.3 && printf XXaaaeee > x.webm && rm x.webm.sq*.part",
        )
        .await;
        assert!(chunks.last().unwrap().is_err());
    }

    #[tokio::test]
    async fn a_finished_download_is_read_whole() {
        assert_eq!(joined(stream_of("printf abc > x.webm").await), b"abc");
    }

    #[tokio::test]
    async fn the_sabr_state_being_saved_is_not_the_media() {
        // the SABR downloader saves its state after each segment through a temporary file
        let chunks = stream_of(format!(
            "printf II > x.webm.sqi.part && printf aaa > x.webm.sq1.part \
             && printf state > tmpritx0d36 && sleep 0.3 && mv tmpritx0d36 x.webm.state \
             && {SABR_JOIN}"
        ))
        .await;
        assert_eq!(joined(chunks), b"IIaaa");
    }

    #[test]
    fn names_tell_the_pieces() {
        let of = |name| Piece::of(name, "x");
        assert_eq!(of("x.webm.sqi.part"), Some(Piece::Init));
        assert_eq!(of("x.webm.sq12.part"), Some(Piece::Run(12)));
        assert_eq!(of("x.webm.sg12.part"), None);
        assert_eq!(of("x.webm.sgi.part"), None);
        assert_eq!(of("x.webm.state"), None);
        assert_eq!(of("x.webm.part"), Some(Piece::Partial));
        assert_eq!(of("x.webm"), Some(Piece::Finished));
    }

    #[test]
    fn only_files_named_after_the_media_count() {
        let of = |name| Piece::of(name, "x");
        assert_eq!(of("tmpritx0d36"), None);
        assert_eq!(of("xy.webm"), None);
        assert_eq!(of("x"), None);
        assert_eq!(of("a.webm"), None);
    }

    #[tokio::test]
    async fn writer_failure_is_an_error() {
        let work = tempfile::tempdir().unwrap();
        let producer = GrowingFileProducer::new(
            work.path().to_owned(),
            "x",
            ProducerCaps::default(),
            None,
            |_, dir: &Path| {
                let script = format!("printf aaa > '{}/x.webm'; exit 3", dir.display());
                (
                    ProcessSpec::new("sh").arg("-c").arg(script),
                    TimestampMode::Absolute,
                )
            },
        );
        let stream = producer.open(None).await.unwrap();
        let chunks: Vec<_> = stream.bytes.collect().await;
        assert!(chunks.last().unwrap().is_err());
    }
}
