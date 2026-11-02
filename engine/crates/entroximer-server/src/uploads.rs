//! Audio files clients upload to play, for sounds that have no URL (`POST /v1/uploads`).
//!
//! Uploads are off unless the operator enables them, since they let anyone who may use the
//! engine store files on it. Each file is streamed to `uploads/` in the run directory, never held
//! in RAM, and read with the demuxer playback uses, so an accepted upload is one that plays. They
//! are gone after a restart.
//!
//! Three limits bound the disk they take: the size of one file, the size of all of them, and how
//! long one is kept after it was last used. When a new upload needs room, expired uploads go
//! first, then those unused for longest. An upload that is playing is never removed: playing
//! holds a [`Lease`] on it. Deleting one that is playing makes it unplayable at once, and its
//! file goes when the playing ends.

use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use entroximer_core::demux::{MediaFacts, inspect};
use entroximer_core::error::MediaError;
use futures_util::{Stream, StreamExt};
use tokio::io::AsyncWriteExt;

/// How often expired uploads are removed when no new upload needs room.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadLimits {
    /// The largest file, in bytes.
    pub max_bytes: u64,
    /// All files together, in bytes.
    pub total_bytes: u64,
    /// How long an upload is kept after it was uploaded or last used.
    pub keep: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("the file is larger than the engine takes ({} MiB)", .max_bytes >> 20)]
    TooLarge { max_bytes: u64 },
    #[error("the engine has no room for this upload until others expire or are deleted")]
    Full,
    #[error("the upload is empty")]
    Empty,
    #[error("the engine cannot play this file")]
    Unplayable(#[source] MediaError),
    #[error("the upload broke off")]
    Body(#[source] io::Error),
    #[error("cannot store the upload")]
    Io(#[source] io::Error),
}

/// An uploaded file.
#[derive(Debug)]
pub struct UploadFile {
    pub id: String,
    pub bytes: u64,
    /// The title the client gave it.
    pub title: Option<String>,
    pub facts: MediaFacts,
    path: PathBuf,
    leases: AtomicUsize,
    last_used: Mutex<Instant>,
}

impl UploadFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn is_leased(&self) -> bool {
        self.leases.load(Ordering::Acquire) > 0
    }

    fn last_used(&self) -> Instant {
        *self.last_used.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    fn is_expired(&self, now: Instant, keep: Duration) -> bool {
        !self.is_leased() && now.saturating_duration_since(self.last_used()) >= keep
    }
}

/// Keeps an upload's file while something plays it. Taking or giving one back counts as a use.
#[derive(Debug)]
pub struct Lease(Arc<UploadFile>);

impl Lease {
    /// Only under the store's lock, so that no sweep sees the file unleased meanwhile.
    fn new(file: Arc<UploadFile>) -> Self {
        file.touch();
        file.leases.fetch_add(1, Ordering::AcqRel);
        Self(file)
    }
}

impl std::ops::Deref for Lease {
    type Target = UploadFile;

    fn deref(&self) -> &UploadFile {
        &self.0
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // before giving it back, so that a sweep never sees it unleased with the old time
        self.0.touch();
        self.0.leases.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub struct UploadStore {
    dir: PathBuf,
    limits: UploadLimits,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Uploads that can be played, by id.
    live: HashMap<String, Arc<UploadFile>>,
    /// Deleted while playing: their files go when the playing ends.
    doomed: Vec<Arc<UploadFile>>,
    /// The size of every file kept, live or doomed.
    stored: u64,
    /// Room promised to uploads streaming in.
    reserved: u64,
}

impl UploadStore {
    pub fn new(dir: PathBuf, limits: UploadLimits) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            limits,
            state: Mutex::default(),
        })
    }

    pub fn limits(&self) -> UploadLimits {
        self.limits
    }

    /// Stores and checks the file `body` streams. `declared` is its size, if the client said.
    pub async fn receive<S, E>(
        &self,
        body: S,
        declared: Option<u64>,
        title: Option<String>,
    ) -> Result<Arc<UploadFile>, UploadError>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: Into<Box<dyn Error + Send + Sync>>,
    {
        let max_bytes = self.limits.max_bytes;
        let too_large = UploadError::TooLarge { max_bytes };
        if declared.is_some_and(|bytes| bytes > max_bytes) {
            return Err(too_large);
        }
        let mut reservation = Reservation {
            store: self,
            bytes: 0,
        };
        if let Some(bytes) = declared {
            reservation.grow(bytes)?;
        }

        let id = uuid::Uuid::new_v4().simple().to_string();
        let partial = Partial(self.dir.join(format!("{id}.part")));
        let mut file = tokio::fs::File::create(&partial.0)
            .await
            .map_err(UploadError::Io)?;
        let mut body = std::pin::pin!(body);
        let mut written = 0;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| UploadError::Body(io::Error::other(e)))?;
            written += chunk.len() as u64;
            if written > max_bytes {
                return Err(too_large);
            }
            if written > reservation.bytes {
                reservation.grow(written - reservation.bytes)?;
            }
            file.write_all(&chunk).await.map_err(UploadError::Io)?;
        }
        file.flush().await.map_err(UploadError::Io)?;
        drop(file);
        if written == 0 {
            return Err(UploadError::Empty);
        }

        let reading = partial.0.clone();
        let facts = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(reading).map_err(MediaError::Source)?;
            // no hint: upload files have no extension, as when they play
            inspect(file, None)
        })
        .await
        .unwrap_or(Err(MediaError::Panicked))
        .map_err(UploadError::Unplayable)?;

        let path = self.dir.join(&id);
        tokio::fs::rename(&partial.0, &path)
            .await
            .map_err(UploadError::Io)?;
        let file = Arc::new(UploadFile {
            id,
            bytes: written,
            title,
            facts,
            path,
            leases: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
        });
        reservation.commit(Arc::clone(&file));
        tracing::debug!(id = file.id, bytes = file.bytes, "stored an upload");
        Ok(file)
    }

    /// The upload `id` for playing, unless it was deleted or has expired.
    pub fn lease(&self, id: &str) -> Option<Lease> {
        let mut state = self.lock();
        if state
            .live
            .get(id)?
            .is_expired(Instant::now(), self.limits.keep)
        {
            let file = state.live.remove(id)?;
            self.remove(&mut state, &file, "expired");
            return None;
        }
        state.live.get(id).cloned().map(Lease::new)
    }

    /// Makes `id` unplayable and frees its room once nothing plays it. Whether it existed.
    pub fn delete(&self, id: &str) -> bool {
        let mut state = self.lock();
        let Some(file) = state.live.remove(id) else {
            return false;
        };
        if file.is_leased() {
            state.doomed.push(file);
        } else {
            self.remove(&mut state, &file, "deleted");
        }
        true
    }

    /// Removes expired uploads, and deleted ones that finished playing.
    pub fn sweep(&self) {
        let mut state = self.lock();
        self.sweep_locked(&mut state);
    }

    /// Sweeps every [`SWEEP_INTERVAL`], for as long as the engine runs.
    pub async fn maintain(self: Arc<Self>) {
        let mut ticks = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            ticks.tick().await;
            self.sweep();
        }
    }

    fn sweep_locked(&self, state: &mut State) {
        let now = Instant::now();
        let expired: Vec<String> = state
            .live
            .values()
            .filter(|file| file.is_expired(now, self.limits.keep))
            .map(|file| file.id.clone())
            .collect();
        for id in expired {
            if let Some(file) = state.live.remove(&id) {
                self.remove(state, &file, "expired");
            }
        }
        let (done, playing) = std::mem::take(&mut state.doomed)
            .into_iter()
            .partition::<Vec<_>, _>(|file| !file.is_leased());
        state.doomed = playing;
        for file in done {
            self.remove(state, &file, "deleted");
        }
    }

    /// Frees at least `bytes`, removing expired uploads, then those unused for longest. Whether
    /// there is room now.
    fn make_room(&self, state: &mut State, bytes: u64) -> bool {
        let free = |state: &State| {
            self.limits
                .total_bytes
                .saturating_sub(state.stored + state.reserved)
        };
        if bytes <= free(state) {
            return true;
        }
        self.sweep_locked(state);
        let mut unused: Vec<(Instant, String)> = state
            .live
            .values()
            .filter(|file| !file.is_leased())
            .map(|file| (file.last_used(), file.id.clone()))
            .collect();
        // the least recently used last, to pop first
        unused.sort_unstable_by(|a, b| b.cmp(a));
        while bytes > free(state) {
            let Some((_, id)) = unused.pop() else {
                return false;
            };
            if let Some(file) = state.live.remove(&id) {
                self.remove(state, &file, "made room for a new upload");
            }
        }
        true
    }

    fn remove(&self, state: &mut State, file: &UploadFile, why: &str) {
        match std::fs::remove_file(&file.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(id = file.id, "cannot remove an upload: {e}"),
        }
        state.stored = state.stored.saturating_sub(file.bytes);
        tracing::debug!(id = file.id, why, "removed an upload");
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Room promised to an upload streaming in, given back unless it is stored.
struct Reservation<'a> {
    store: &'a UploadStore,
    bytes: u64,
}

impl Reservation<'_> {
    fn grow(&mut self, bytes: u64) -> Result<(), UploadError> {
        let mut state = self.store.lock();
        if !self.store.make_room(&mut state, bytes) {
            return Err(UploadError::Full);
        }
        state.reserved += bytes;
        self.bytes += bytes;
        Ok(())
    }

    /// Counts the file instead of the reservation.
    fn commit(mut self, file: Arc<UploadFile>) {
        let mut state = self.store.lock();
        state.reserved -= self.bytes;
        self.bytes = 0;
        state.stored += file.bytes;
        state.live.insert(file.id.clone(), file);
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.store.lock().reserved -= self.bytes;
        }
    }
}

/// A file being written, removed unless it was moved away.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %self.0.display(), "cannot remove a partial upload: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use entroximer_core::timeline::TICKS_PER_SECOND;

    use super::*;

    const WAV_BYTES: u64 = 2 * 48_000 * 2 * 2 + 44;

    /// Two seconds of a stereo tone as a WAV file.
    fn wav() -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut out = Cursor::new(Vec::new());
        let mut writer = hound::WavWriter::new(&mut out, spec).unwrap();
        for i in 0..2 * 48_000 {
            let sample =
                ((i as f32 * 440.0 / 48_000.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            writer.write_sample(sample).unwrap();
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        out.into_inner()
    }

    /// `data` streamed in chunks of 64 KiB.
    fn body(data: &[u8]) -> impl Stream<Item = Result<Bytes, io::Error>> + use<> {
        let chunks: Vec<_> = data
            .chunks(64 << 10)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        futures_util::stream::iter(chunks)
    }

    fn store(dir: &Path, total_bytes: u64, keep: Duration) -> UploadStore {
        let limits = UploadLimits {
            max_bytes: 1 << 20,
            total_bytes,
            keep,
        };
        UploadStore::new(dir.join("uploads"), limits).unwrap()
    }

    /// The names of the files in the store's directory.
    fn files(store: &UploadStore) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(&store.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    async fn upload(store: &UploadStore) -> Arc<UploadFile> {
        store.receive(body(&wav()), None, None).await.unwrap()
    }

    #[tokio::test]
    async fn keeps_what_plays_and_nothing_of_what_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 4 << 20, Duration::from_secs(60));
        let file = store
            .receive(body(&wav()), Some(WAV_BYTES), Some("Beep".into()))
            .await
            .unwrap();
        assert_eq!(file.bytes, WAV_BYTES);
        assert_eq!(file.facts.length, 2 * TICKS_PER_SECOND);
        assert_eq!(file.title.as_deref(), Some("Beep"));
        assert_eq!(files(&store), [file.id.as_str()]);

        let text = b"not audio at all".repeat(1000);
        let refused = store.receive(body(&text), None, None).await;
        assert!(
            matches!(refused, Err(UploadError::Unplayable(_))),
            "{refused:?}"
        );
        let refused = store.receive(body(&[]), None, None).await;
        assert!(matches!(refused, Err(UploadError::Empty)), "{refused:?}");
        let big = vec![0; (1 << 20) + 1];
        let refused = store
            .receive(body(&big), Some(big.len() as u64), None)
            .await;
        assert!(
            matches!(refused, Err(UploadError::TooLarge { .. })),
            "{refused:?}"
        );
        // without a declared size, it is noticed while streaming
        let refused = store.receive(body(&big), None, None).await;
        assert!(
            matches!(refused, Err(UploadError::TooLarge { .. })),
            "{refused:?}"
        );

        assert_eq!(files(&store), [file.id.as_str()], "no partial file is left");
        let state = store.lock();
        assert_eq!((state.stored, state.reserved), (WAV_BYTES, 0));
    }

    #[tokio::test]
    async fn uploads_expire_unless_playing() {
        let dir = tempfile::tempdir().unwrap();
        let keep = Duration::from_millis(100);
        let store = store(dir.path(), 4 << 20, keep);
        let playing = upload(&store).await;
        let idle = upload(&store).await;
        let lease = store.lease(&playing.id).unwrap();

        tokio::time::sleep(keep * 2).await;
        store.sweep();
        assert_eq!(files(&store), [playing.id.as_str()]);
        assert!(store.lease(&idle.id).is_none());

        // kept for a while after the playing ends, as after an upload
        drop(lease);
        store.sweep();
        assert!(store.lease(&playing.id).is_some());
        tokio::time::sleep(keep * 2).await;
        assert!(
            store.lease(&playing.id).is_none(),
            "expired once unused for long enough"
        );
        assert!(files(&store).is_empty());
        assert_eq!(store.lock().stored, 0);
    }

    #[tokio::test]
    async fn room_is_made_from_the_uploads_unused_for_longest() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), WAV_BYTES * 5 / 2, Duration::from_secs(60));
        let first = upload(&store).await;
        let second = upload(&store).await;
        drop(store.lease(&first.id));

        let third = upload(&store).await;
        assert!(
            store.lease(&second.id).is_none(),
            "the least recently used goes"
        );
        let (_a, _b) = (
            store.lease(&first.id).unwrap(),
            store.lease(&third.id).unwrap(),
        );

        let refused = store.receive(body(&wav()), None, None).await;
        assert!(matches!(refused, Err(UploadError::Full)), "{refused:?}");
        let mut expected = vec![first.id.clone(), third.id.clone()];
        expected.sort();
        assert_eq!(files(&store), expected, "nothing playing was removed");
        let state = store.lock();
        assert_eq!((state.stored, state.reserved), (2 * WAV_BYTES, 0));
    }

    #[tokio::test]
    async fn a_deleted_upload_stops_being_playable_and_goes_when_its_playing_ends() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), WAV_BYTES * 3 / 2, Duration::from_secs(60));
        let file = upload(&store).await;
        let lease = store.lease(&file.id).unwrap();

        assert!(store.delete(&file.id));
        assert!(!store.delete(&file.id), "deleted once");
        assert!(store.lease(&file.id).is_none());
        store.sweep();
        assert!(lease.path().exists(), "still playing");
        let refused = store.receive(body(&wav()), None, None).await;
        assert!(
            matches!(refused, Err(UploadError::Full)),
            "its room is still taken"
        );

        drop(lease);
        let next = upload(&store).await;
        assert_eq!(files(&store), [next.id.as_str()]);
    }
}
