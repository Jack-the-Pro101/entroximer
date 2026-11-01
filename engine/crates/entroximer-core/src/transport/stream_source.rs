use std::io;

use bytes::{Buf, Bytes};
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use kanal::Receiver;

// TODO: #![feature(type_alias_impl_trait)]
// pub type WebMediaDownload<'a> = impl Future<Output = io::Result<()>> + Send + 'a;
pub type WebMediaDownload<'a> = BoxFuture<'a, io::Result<()>>;

pub struct WebMediaSource {
    rx: Receiver<io::Result<bytes::Bytes>>,
    cur: Option<io::Cursor<bytes::Bytes>>,
}

// TODO: #![feature(type_alias_impl_trait)]
// #[define_opaque(WebMediaDownload)]
fn new_download_fut<'a>(
    mut stream: BoxStream<'a, io::Result<Bytes>>,
    tx: kanal::AsyncSender<io::Result<Bytes>>,
) -> WebMediaDownload<'a> {
    Box::pin(async move {
        while let Some(chunk_res) = stream.next().await {
            match tx.send(chunk_res).await {
                Ok(()) => {}
                Err(kanal::SendError::Closed | kanal::SendError::ReceiveClosed) => break,
            }
        }
        Ok(())
    })
}

// Seeking is intentionally not supported here: the demuxer reads every source forward-only and
// seeks happen on the packet timeline instead (see `crate::timeline`).
pub fn stream_media<'a, S>(stream: S) -> (WebMediaDownload<'a>, WebMediaSource)
where
    S: Stream<Item = io::Result<Bytes>> + Send + 'a,
{
    let (tx, rx) = kanal::bounded_async(4);
    let rx = rx.to_sync();
    let download: WebMediaDownload = new_download_fut(Box::pin(stream), tx);
    (download, WebMediaSource { rx, cur: None })
}

impl std::fmt::Debug for WebMediaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebMediaSource").finish_non_exhaustive()
    }
}

impl io::Read for WebMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // an empty chunk must not be mistaken for the end of the stream
        while self.cur.as_ref().is_none_or(|cur| !cur.has_remaining()) {
            let chunk = match self.rx.recv() {
                Ok(chunk) => chunk?,
                // The channel only ends by the download dropping its sender: stream errors arrive
                // in-band as `Err` chunks above. kanal reports a sender dropped while we were
                // blocked as `Closed` rather than `SendClosed`, so both mean end of stream.
                Err(kanal::ReceiveError::SendClosed | kanal::ReceiveError::Closed) => return Ok(0),
            };
            self.cur = Some(io::Cursor::new(chunk));
        }
        self.cur
            .as_mut()
            .expect("the loop exits with a non-empty chunk")
            .read(buf)
    }
}

impl io::Seek for WebMediaSource {
    fn seek(&mut self, _: io::SeekFrom) -> io::Result<u64> {
        Err(io::Error::other("source does not support seeking"))
    }
}

impl symphonia::core::io::MediaSource for WebMediaSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        // TODO: when known via Content-Length
        None
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[tokio::test]
    async fn it_works() {
        let data = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magnam aliquam quaerat voluptatem. ut enim aeque doleamus animo, cum corpore dolemus, fieri tamen permagna accessio potest, si aliquod aeternum et⏎";
        let stream = futures_util::stream::iter(
            data.as_bytes()
                .chunks(16)
                .map(|chunk| Ok(Bytes::from_static(chunk))),
        );
        let (download, mut source) = stream_media(stream);

        let (a_r, b_r) = futures_util::future::join(
            download,
            tokio::task::spawn_blocking(move || {
                let mut buf = [0u8; 64];
                assert_eq!(source.read(&mut buf)?, 16);
                assert_eq!(buf[0..16], data.as_bytes()[0..16]);
                source.read_exact(&mut buf[16..])?;
                assert_eq!(buf, data.as_bytes()[0..64]);
                io::Result::Ok(())
            }),
        )
        .await;

        a_r.expect("the downloader errored");
        b_r.expect("the decoder thread panicked")
            .expect("the decoder thread errored");
    }

    #[tokio::test]
    async fn finished_stream_reads_as_eof_and_errors_propagate() {
        let ok = futures_util::stream::iter([Ok(Bytes::from_static(b"abc"))]);
        let (download, mut source) = stream_media(ok);
        let (a_r, b_r) = futures_util::future::join(
            download,
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                source.read_to_end(&mut out).map(|_| out)
            }),
        )
        .await;
        a_r.unwrap();
        assert_eq!(b_r.unwrap().unwrap(), b"abc");

        let failing = futures_util::stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Err(io::Error::other("producer died")),
        ]);
        let (download, mut source) = stream_media(failing);
        let (_, b_r) = futures_util::future::join(
            download,
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                source.read_to_end(&mut out)
            }),
        )
        .await;
        assert!(b_r.unwrap().is_err());
    }
}
