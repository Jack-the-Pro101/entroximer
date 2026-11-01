//! Bridges a producer's async byte stream to the blocking [`Read`](io::Read) the demuxer needs.
//!
//! [`stream_reader`] splits a stream into a pump, which runs on the async runtime, and a
//! [`StreamReader`] for the demuxer's blocking thread. They share a channel of a few chunks, so
//! a demuxer that falls behind (e.g. while the timeline applies backpressure) pauses the
//! download instead of buffering it.
//!
//! Seeking is deliberately unsupported: every source is read forward-only, and seeks happen on
//! the packet timeline (see [`crate::timeline`]).

use std::future::Future;
use std::io;

use bytes::{Buf, Bytes};
use futures_util::{Stream, StreamExt};
use kanal::{ReceiveError, Receiver, SendError};

/// Chunks buffered between the pump and the reader.
const CHANNEL_CHUNKS: usize = 4;

/// Returns the pump, which completes once `stream` ends or the reader is dropped, and the
/// reader. Errors of the stream reach the reader in order, as errors of its reads.
pub fn stream_reader<'a, S>(stream: S) -> (impl Future<Output = ()> + Send + 'a, StreamReader)
where
    S: Stream<Item = io::Result<Bytes>> + Send + 'a,
{
    let (tx, rx) = kanal::bounded_async(CHANNEL_CHUNKS);
    let pump = async move {
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.next().await {
            match tx.send(chunk).await {
                Ok(()) => {}
                Err(SendError::Closed | SendError::ReceiveClosed) => break,
            }
        }
    };
    let reader = StreamReader {
        chunks: rx.to_sync(),
        current: Bytes::new(),
    };
    (pump, reader)
}

/// The reading half of [`stream_reader`].
pub struct StreamReader {
    chunks: Receiver<io::Result<Bytes>>,
    /// What is left of the chunk being read.
    current: Bytes,
}

impl std::fmt::Debug for StreamReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamReader")
            .field("buffered", &self.current.len())
            .finish_non_exhaustive()
    }
}

impl io::Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // an empty chunk must not be mistaken for the end of the stream
        while !self.current.has_remaining() {
            match self.chunks.recv() {
                Ok(chunk) => self.current = chunk?,
                // The channel only ends by the pump dropping its sender: stream errors arrive
                // in-band above. kanal reports a sender dropped while we were blocked as
                // `Closed` rather than `SendClosed`, so both mean the end of the stream.
                Err(ReceiveError::SendClosed | ReceiveError::Closed) => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len());
        self.current.copy_to_slice(&mut buf[..n]);
        Ok(n)
    }
}

impl io::Seek for StreamReader {
    fn seek(&mut self, _: io::SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streams are read forward-only; seeking happens on the timeline",
        ))
    }
}

impl symphonia::core::io::MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    /// Runs the pump while `read` consumes the reader on a blocking thread.
    async fn pump_and_read<T: Send + 'static>(
        chunks: Vec<io::Result<Bytes>>,
        read: impl FnOnce(StreamReader) -> T + Send + 'static,
    ) -> T {
        let (pump, reader) = stream_reader(futures_util::stream::iter(chunks));
        let reading = tokio::task::spawn_blocking(move || read(reader));
        let ((), read) = futures_util::future::join(pump, reading).await;
        read.expect("the reading thread panicked")
    }

    #[tokio::test]
    async fn reads_across_chunk_boundaries() {
        let data: Vec<u8> = (0..=255).cycle().take(100).collect();
        let chunks = data
            .chunks(16)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let read = pump_and_read(chunks, |mut reader| {
            let mut first = [0u8; 64];
            // a read returns at most what is left of the current chunk
            assert_eq!(reader.read(&mut first).unwrap(), 16);
            reader.read_exact(&mut first[16..]).unwrap();
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).unwrap();
            [first.to_vec(), rest].concat()
        })
        .await;
        assert_eq!(read, data);
    }

    #[tokio::test]
    async fn empty_chunks_are_skipped_and_errors_arrive_in_order() {
        let chunks = vec![
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"abc")),
            Err(io::Error::other("producer died")),
        ];
        let (data, error) = pump_and_read(chunks, |mut reader| {
            let mut data = [0u8; 3];
            reader.read_exact(&mut data).unwrap();
            (data, reader.read(&mut [0u8; 8]).unwrap_err())
        })
        .await;
        assert_eq!(&data, b"abc");
        assert_eq!(error.to_string(), "producer died");

        let ended = pump_and_read(vec![Ok(Bytes::from_static(b"x"))], |mut reader| {
            let mut out = Vec::new();
            reader.read_to_end(&mut out).map(|_| out)
        })
        .await;
        assert_eq!(ended.unwrap(), b"x");
    }
}
