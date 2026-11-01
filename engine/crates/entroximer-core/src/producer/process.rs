//! Child processes whose stdout is the media, e.g. `yt-dlp ... -o -`.

use std::collections::VecDeque;
use std::io;
use std::process::Stdio;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr};
use tokio::task::JoinHandle;
use tokio_util::io::ReaderStream;

use super::{Producer, ProducerCaps, ProducerStream, READ_CHUNK, TimestampMode};
use crate::timeline::Ticks;

#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Written to the child's stdin, which is then closed.
    pub stdin: Option<Bytes>,
}

impl ProcessSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdin: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn stdin(mut self, data: Bytes) -> Self {
        self.stdin = Some(data);
        self
    }

    /// Spawns the process with stdout as `stdout`; stderr is logged and its tail kept for errors.
    pub(crate) fn spawn(&self, stdout: Stdio) -> io::Result<RunningProcess> {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args)
            .stdout(stdout)
            .stderr(Stdio::piped())
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            io::Error::new(e.kind(), format!("failed to spawn {}: {e}", self.program))
        })?;
        if let (Some(data), Some(mut stdin)) = (self.stdin.clone(), child.stdin.take()) {
            tokio::spawn(async move {
                let _ = stdin.write_all(&data).await;
            });
        }
        let program: Arc<str> = self.program.as_str().into();
        let stderr = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(collect_stderr(stderr, program.clone())));
        Ok(RunningProcess {
            child,
            stderr,
            program,
        })
    }
}

/// A spawned child; killed when dropped.
pub(crate) struct RunningProcess {
    pub(crate) child: Child,
    stderr: Option<JoinHandle<String>>,
    program: Arc<str>,
}

impl RunningProcess {
    /// Waits for exit; a failure becomes an error carrying the end of stderr.
    pub(crate) async fn finish(&mut self) -> io::Result<()> {
        let status = self.child.wait().await?;
        let tail = match self.stderr.take() {
            Some(h) => h.await.unwrap_or_default(),
            None => String::new(),
        };
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{} exited with {status}: {}",
                self.program,
                tail.trim()
            )))
        }
    }
}

async fn collect_stderr(stderr: ChildStderr, program: Arc<str>) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut tail = VecDeque::with_capacity(16);
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(target: "entroximer::producer", "{program}: {line}");
        if tail.len() == 16 {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    Vec::from(tail).join("\n")
}

/// Streams a child's stdout. A non-zero exit becomes a final `Err` item, so a producer that dies
/// half-way is never mistaken for the end of the media.
pub fn stdout_stream(spec: &ProcessSpec) -> io::Result<BoxStream<'static, io::Result<Bytes>>> {
    let mut proc = spec.spawn(Stdio::piped())?;
    let stdout = proc.child.stdout.take().expect("stdout is piped");
    let state = (ReaderStream::with_capacity(stdout, READ_CHUNK), proc, false);
    Ok(
        futures_util::stream::unfold(state, |(mut out, mut proc, done)| async move {
            if done {
                return None;
            }
            match out.next().await {
                Some(item) => Some((item, (out, proc, false))),
                None => match proc.finish().await {
                    Ok(()) => None,
                    Err(e) => Some((Err(e), (out, proc, true))),
                },
            }
        })
        .boxed(),
    )
}

/// A producer that runs a command per `open`, built from the requested start time.
pub struct ProcessProducer<F> {
    build: F,
    caps: ProducerCaps,
    hint: Option<String>,
}

impl<F> ProcessProducer<F>
where
    F: Fn(Option<Ticks>) -> (ProcessSpec, TimestampMode) + Send + Sync + 'static,
{
    pub fn new(caps: ProducerCaps, hint: Option<String>, build: F) -> Self {
        Self { build, caps, hint }
    }
}

impl<F> std::fmt::Debug for ProcessProducer<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessProducer")
            .field("caps", &self.caps)
            .field("hint", &self.hint)
            .finish_non_exhaustive()
    }
}

impl<F> Producer for ProcessProducer<F>
where
    F: Fn(Option<Ticks>) -> (ProcessSpec, TimestampMode) + Send + Sync + 'static,
{
    fn caps(&self) -> ProducerCaps {
        self.caps
    }

    fn open(&self, start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        let (spec, timestamps) = (self.build)(start);
        let hint = self.hint.clone();
        Box::pin(async move {
            Ok(ProducerStream {
                bytes: stdout_stream(&spec)?,
                timestamps,
                hint,
            })
        })
    }
}
