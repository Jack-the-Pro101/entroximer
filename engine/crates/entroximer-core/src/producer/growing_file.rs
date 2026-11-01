//! Streams a file that another process is still writing.
//!
//! yt-dlp's SABR downloader (yt-dlp PR #13515) cannot write to stdout, so it is pointed at an
//! empty per-open directory and the first media file appearing there is tailed until the
//! process exits. The open file descriptor survives yt-dlp renaming `.part` to the final name.

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
    caps: ProducerCaps,
    hint: Option<String>,
    pub poll: Duration,
}

impl<F> GrowingFileProducer<F>
where
    F: Fn(Option<Ticks>, &Path) -> (ProcessSpec, TimestampMode) + Send + Sync + 'static,
{
    /// `build` receives the requested start and an empty directory to write into, which is
    /// created under `work_dir` and removed when the stream is dropped.
    pub fn new(work_dir: PathBuf, caps: ProducerCaps, hint: Option<String>, build: F) -> Self {
        Self {
            build,
            work_dir,
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
        let (hint, poll) = (self.hint.clone(), self.poll);
        Box::pin(async move {
            let (dir, proc, timestamps) = opened?;
            let tail = Tail {
                dir,
                proc,
                file: None,
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
    proc: RunningProcess,
    file: Option<tokio::fs::File>,
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
            let exited = match self.proc.child.try_wait() {
                Ok(status) => status.is_some(),
                Err(e) => return self.fail(e),
            };
            let Some(file) = &mut self.file else {
                match find_media_file(self.dir.path()).await {
                    Ok(Some(path)) => match tokio::fs::File::open(&path).await {
                        Ok(f) => self.file = Some(f),
                        // renamed between listing and opening: look again
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return self.fail(e),
                    },
                    Ok(None) if exited => {
                        self.done = true;
                        let err = match self.proc.finish().await {
                            Ok(()) => io::Error::other("producer exited without writing media"),
                            Err(e) => e,
                        };
                        return Some(Err(err));
                    }
                    Ok(None) => tokio::time::sleep(self.poll).await,
                    Err(e) => return self.fail(e),
                }
                continue;
            };
            match file.read(&mut buf).await {
                Ok(0) if exited => {
                    // the writer is gone and everything it wrote has been read
                    self.done = true;
                    return self.proc.finish().await.err().map(Err);
                }
                Ok(0) => tokio::time::sleep(self.poll).await,
                Ok(n) => {
                    buf.truncate(n);
                    return Some(Ok(Bytes::from(buf)));
                }
                Err(e) => return self.fail(e),
            }
        }
    }

    fn fail(&mut self, e: io::Error) -> Option<io::Result<Bytes>> {
        self.done = true;
        Some(Err(e))
    }
}

async fn find_media_file(dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut found = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().await?.is_file() && !IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s))
        {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found.into_iter().next())
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

    #[tokio::test]
    async fn writer_failure_is_an_error() {
        let work = tempfile::tempdir().unwrap();
        let producer = GrowingFileProducer::new(
            work.path().to_owned(),
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
