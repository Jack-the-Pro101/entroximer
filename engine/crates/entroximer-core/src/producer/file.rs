//! A local file read front to back. Use `FfmpegProducer` when time seeking is wanted.

use std::io;
use std::path::PathBuf;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use tokio_util::io::ReaderStream;

use super::{Producer, ProducerCaps, ProducerStream, TimestampMode};
use crate::timeline::Ticks;

#[derive(Clone, Debug)]
pub struct FileProducer {
    pub path: PathBuf,
}

impl Producer for FileProducer {
    fn caps(&self) -> ProducerCaps {
        ProducerCaps::default()
    }

    fn open(&self, _start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        let path = self.path.clone();
        Box::pin(async move {
            let file = tokio::fs::File::open(&path).await?;
            Ok(ProducerStream {
                bytes: ReaderStream::new(file).boxed(),
                timestamps: TimestampMode::Absolute,
                hint: path.extension().map(|e| e.to_string_lossy().into_owned()),
            })
        })
    }
}
