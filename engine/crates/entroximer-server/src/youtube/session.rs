//! The engine's YouTube session: cookies exported from a signed-in browser session, stored in
//! the data directory and shared safely by every yt-dlp process.
//!
//! yt-dlp writes its cookie file back when it exits, including cookies YouTube rotated. Each
//! process therefore gets its own copy ([`SessionCopy`]), dropped as soon as the process has
//! exited: whatever changed in it is merged into the stored session. Concurrent processes never
//! overwrite each other, and rotations keep the session alive. A copy of a session that has
//! since been replaced (re-imported or removed) is discarded instead.
//!
//! Copies must be short-lived: sending cookies YouTube has since rotated looks like a stolen
//! session, which YouTube ends. A track keeps a [`Session`] instead, which hands each of its downloads a fresh
//! copy.
//!
//! Files, readable by the engine's user only:
//! - `cookies.txt`: the session, in the Netscape format yt-dlp uses;
//! - `session.json`: state and timestamps;
//! - `session.lock`: serialises writers across processes, e.g. the server and the CLI;
//! - `rotation.lock`: held while a browser rotates the session, so only one does.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::sync::broadcast;

use super::cookies::{CookieJar, ParseError};

const COOKIES_FILE: &str = "cookies.txt";
const META_FILE: &str = "session.json";
const LOCK_FILE: &str = "session.lock";
const ROTATION_LOCK_FILE: &str = "rotation.lock";
/// Sites whose cookies are kept on import; yt-dlp needs nothing else.
const SITES: &[&str] = &["youtube.com", "google.com"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    /// No session has been imported.
    #[default]
    None,
    /// Imported, not checked yet.
    Unverified,
    Valid,
    /// YouTube no longer accepts the session; fresh cookies must be imported.
    Expired,
}

impl SessionState {
    /// Whether the session is worth offering to yt-dlp.
    pub fn usable(self) -> bool {
        matches!(self, Self::Unverified | Self::Valid)
    }
}

/// Result of `GET /v1/youtube/session`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionStatus {
    pub state: SessionState,
    /// Unix seconds.
    #[ts(type = "number | null")]
    pub imported_at: Option<u64>,
    /// Unix seconds.
    #[ts(type = "number | null")]
    pub checked_at: Option<u64>,
    /// When a browser last rotated the cookies, in Unix seconds.
    #[ts(type = "number | null")]
    pub rotated_at: Option<u64>,
    /// When the earliest cookie the session depends on expires, in Unix seconds.
    #[ts(type = "number | null")]
    pub expires_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub cookies: usize,
    /// Cookies of other sites, left out of the session.
    pub dropped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("cannot access {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("not a Netscape cookie file")]
    Format(#[from] ParseError),
    #[error(
        "these cookies are not from a signed-in YouTube session (LOGIN_INFO or SAPISID is missing)"
    )]
    NotSignedIn,
    #[error("the session metadata is corrupt")]
    Meta(#[source] serde_json::Error),
}

/// Where rotating the session stands, for scheduling the next rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RotationState {
    pub generation: u64,
    pub state: SessionState,
    /// Unix seconds.
    pub rotated_at: Option<u64>,
    /// Times in a row YouTube refused to rotate the session.
    pub refusals: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Meta {
    /// Increases whenever the session is replaced, so copies of an old one never merge back.
    generation: u64,
    state: SessionState,
    imported_at: Option<u64>,
    checked_at: Option<u64>,
    /// Fingerprint of the file last imported by `import_file_if_changed`.
    source_fingerprint: Option<u64>,
    rotated_at: Option<u64>,
    /// Refusals in a row; see [`RotationState::refusals`].
    rotation_refusals: u32,
}

#[derive(Debug)]
pub struct SessionStore {
    dir: PathBuf,
    /// Changes of the state made by this process.
    changes: broadcast::Sender<SessionState>,
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            changes: broadcast::Sender::new(16),
        }
    }

    /// Changes of the session's state made by this process: imports, removals, and what using,
    /// checking or rotating the session showed.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionState> {
        self.changes.subscribe()
    }

    /// Replaces the session with the cookies in `text`.
    pub fn import(&self, text: &str) -> Result<ImportReport, SessionError> {
        let _lock = self.lock()?;
        self.import_locked(text, None)
    }

    fn changed(&self, state: SessionState) {
        // nobody listening (e.g. in the CLI) is fine
        let _ = self.changes.send(state);
    }

    /// Imports `path` unless its contents are what was imported from it last time, so that a
    /// mounted secret does not overwrite cookies rotated since. `None` when nothing changed.
    pub fn import_file_if_changed(
        &self,
        path: &Path,
    ) -> Result<Option<ImportReport>, SessionError> {
        let text = fs::read_to_string(path).map_err(io_error(path))?;
        let fingerprint = fnv1a(text.as_bytes());
        let _lock = self.lock()?;
        if self.read_meta()?.source_fingerprint == Some(fingerprint) {
            return Ok(None);
        }
        self.import_locked(&text, Some(fingerprint)).map(Some)
    }

    /// Signs out; returns whether there was a session.
    pub fn remove(&self) -> Result<bool, SessionError> {
        let _lock = self.lock()?;
        let meta = self.read_meta()?;
        let path = self.path(COOKIES_FILE);
        let existed = match fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(io_error(&path)(e)),
        };
        self.write_meta(&Meta {
            generation: meta.generation + 1,
            source_fingerprint: meta.source_fingerprint,
            ..Meta::default()
        })?;
        if meta.state != SessionState::None {
            self.changed(SessionState::None);
        }
        Ok(existed)
    }

    pub fn status(&self) -> Result<SessionStatus, SessionError> {
        let _lock = self.lock()?;
        let meta = self.read_meta()?;
        let expires_at = self.read_jar()?.and_then(|jar| jar.session_expiry());
        Ok(SessionStatus {
            state: meta.state,
            imported_at: meta.imported_at,
            checked_at: meta.checked_at,
            rotated_at: meta.rotated_at,
            expires_at,
        })
    }

    pub fn rotation(&self) -> Result<RotationState, SessionError> {
        let _lock = self.lock()?;
        let meta = self.read_meta()?;
        Ok(RotationState {
            generation: meta.generation,
            state: meta.state,
            rotated_at: meta.rotated_at,
            refusals: meta.rotation_refusals,
        })
    }

    /// Records that a browser rotated the session (of `generation`); returns its state, or
    /// `None` when the session has been replaced since.
    pub fn rotated(&self, generation: u64) -> Result<Option<SessionState>, SessionError> {
        self.update_rotation(generation, |meta| {
            meta.rotated_at = Some(unix_now());
            meta.rotation_refusals = 0;
        })
        .map(|meta| meta.map(|m| m.state))
    }

    /// Records that YouTube refused to rotate the session (of `generation`); returns how many
    /// times in a row it has, or `None` when the session has been replaced since.
    pub fn refused(&self, generation: u64) -> Result<Option<u32>, SessionError> {
        self.update_rotation(generation, |meta| {
            meta.rotation_refusals = meta.rotation_refusals.saturating_add(1);
        })
        .map(|meta| meta.map(|m| m.rotation_refusals))
    }

    fn update_rotation(
        &self,
        generation: u64,
        update: impl FnOnce(&mut Meta),
    ) -> Result<Option<Meta>, SessionError> {
        let _lock = self.lock()?;
        let mut meta = self.read_meta()?;
        if meta.generation != generation || meta.state == SessionState::None {
            return Ok(None);
        }
        update(&mut meta);
        self.write_meta(&meta)?;
        Ok(Some(meta))
    }

    /// The right to rotate the session, held until the file is dropped; `None` while another
    /// process (or task) rotates it.
    pub fn try_lock_rotation(&self) -> Result<Option<File>, SessionError> {
        let file = self.lock_file(ROTATION_LOCK_FILE)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(io_error(&self.path(ROTATION_LOCK_FILE))(e)),
        }
    }

    /// Records what using or checking the session (of `generation`) showed. Ignored when the
    /// session has been replaced since. Returns the previous state if it changed.
    pub fn record(
        &self,
        generation: u64,
        state: SessionState,
        checked: bool,
    ) -> Result<Option<SessionState>, SessionError> {
        let _lock = self.lock()?;
        let mut meta = self.read_meta()?;
        if meta.generation != generation || meta.state == SessionState::None {
            return Ok(None);
        }
        if checked {
            meta.checked_at = Some(unix_now());
        }
        let previous = std::mem::replace(&mut meta.state, state);
        self.write_meta(&meta)?;
        if previous == state {
            return Ok(None);
        }
        self.changed(state);
        Ok(Some(previous))
    }

    /// A private copy of the session for one yt-dlp process, created in `work_dir`; `None`
    /// when there is no usable session. Changes are merged back when the copy is dropped.
    pub fn checkout(
        self: &Arc<Self>,
        work_dir: &Path,
    ) -> Result<Option<SessionCopy>, SessionError> {
        self.checkout_if(work_dir, |meta| meta.state.usable())
    }

    /// Like [`checkout`](Self::checkout), but also for a session considered expired, to check
    /// it again.
    pub fn checkout_any(
        self: &Arc<Self>,
        work_dir: &Path,
    ) -> Result<Option<SessionCopy>, SessionError> {
        self.checkout_if(work_dir, |meta| meta.state != SessionState::None)
    }

    fn checkout_if(
        self: &Arc<Self>,
        work_dir: &Path,
        accept: impl FnOnce(&Meta) -> bool,
    ) -> Result<Option<SessionCopy>, SessionError> {
        let (meta, jar) = {
            let _lock = self.lock()?;
            (self.read_meta()?, self.read_jar()?)
        };
        let Some(jar) = jar.filter(|_| accept(&meta)) else {
            return Ok(None);
        };
        // created readable by the engine's user only
        let mut file = tempfile::Builder::new()
            .prefix("youtube-cookies-")
            .suffix(".txt")
            .tempfile_in(work_dir)
            .map_err(io_error(work_dir))?;
        file.write_all(jar.to_string().as_bytes())
            .map_err(io_error(file.path()))?;
        Ok(Some(SessionCopy {
            store: Arc::clone(self),
            file,
            base: jar,
            generation: meta.generation,
        }))
    }

    fn import_locked(
        &self,
        text: &str,
        fingerprint: Option<u64>,
    ) -> Result<ImportReport, SessionError> {
        let mut jar: CookieJar = text.parse()?;
        let dropped = jar.retain_sites(SITES);
        if !jar.has_youtube_login() {
            return Err(SessionError::NotSignedIn);
        }
        let previous = self.read_meta()?;
        self.write_atomically(COOKIES_FILE, jar.to_string().as_bytes())?;
        self.write_meta(&Meta {
            generation: previous.generation + 1,
            state: SessionState::Unverified,
            imported_at: Some(unix_now()),
            checked_at: None,
            // a manual import keeps the secret's fingerprint: the secret only wins again once
            // its contents change
            source_fingerprint: fingerprint.or(previous.source_fingerprint),
            rotated_at: None,
            rotation_refusals: 0,
        })?;
        self.changed(SessionState::Unverified);
        Ok(ImportReport {
            cookies: jar.len(),
            dropped,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn lock(&self) -> Result<File, SessionError> {
        let file = self.lock_file(LOCK_FILE)?;
        file.lock().map_err(io_error(&self.path(LOCK_FILE)))?;
        Ok(file)
    }

    fn lock_file(&self, name: &str) -> Result<File, SessionError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(io_error(&self.dir))?;
        let path = self.path(name);
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(io_error(&path))
    }

    fn read_meta(&self) -> Result<Meta, SessionError> {
        let path = self.path(META_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(SessionError::Meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Meta::default()),
            Err(e) => Err(io_error(&path)(e)),
        }
    }

    fn write_meta(&self, meta: &Meta) -> Result<(), SessionError> {
        let json = serde_json::to_vec_pretty(meta).map_err(SessionError::Meta)?;
        self.write_atomically(META_FILE, &json)
    }

    fn read_jar(&self) -> Result<Option<CookieJar>, SessionError> {
        let path = self.path(COOKIES_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text.parse()?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error(&path)(e)),
        }
    }

    fn write_atomically(&self, name: &str, contents: &[u8]) -> Result<(), SessionError> {
        let path = self.path(name);
        let staged = self.path(&format!("{name}.new"));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&staged)
            .map_err(io_error(&staged))?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(io_error(&staged))?;
        fs::rename(&staged, &path).map_err(io_error(&path))
    }
}

/// The session a track was resolved with, kept for its downloads.
#[derive(Clone, Debug)]
pub struct Session {
    store: Arc<SessionStore>,
    generation: u64,
}

impl Session {
    /// A copy of the stored cookies, as rotated so far, for one yt-dlp process; `None` once the
    /// session has been replaced or removed.
    pub fn checkout(&self, work_dir: &Path) -> Result<Option<SessionCopy>, SessionError> {
        self.store
            .checkout_if(work_dir, |meta| meta.generation == self.generation)
    }
}

/// A private copy of the session handed to one yt-dlp process (`--cookies <path>`).
pub struct SessionCopy {
    store: Arc<SessionStore>,
    file: NamedTempFile,
    base: CookieJar,
    generation: u64,
}

impl SessionCopy {
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The session this is a copy of, to check out again for later processes.
    pub fn session(&self) -> Session {
        Session {
            store: Arc::clone(&self.store),
            generation: self.generation,
        }
    }

    /// The session's cookies as they were copied.
    pub fn base(&self) -> &CookieJar {
        &self.base
    }

    /// Writes into the copy what changed from `before` to `after` (e.g. what a browser did with
    /// the cookies it took), leaving out other sites. Merged into the session when the copy is
    /// dropped, like yt-dlp's changes; cookies missing from `before` (ones the browser would not
    /// take) are left as they are.
    pub fn absorb(&self, before: &CookieJar, after: &CookieJar) -> Result<(), SessionError> {
        let mut before = before.clone();
        let mut after = after.clone();
        before.retain_sites(SITES);
        after.retain_sites(SITES);
        let mut jar = self.base.clone();
        jar.apply(&after.changes_since(&before));
        fs::write(self.path(), jar.to_string()).map_err(io_error(self.path()))
    }

    /// Applies what yt-dlp changed in the copy to the stored session; returns how many cookies
    /// changed.
    fn merge_back(&self) -> Result<usize, SessionError> {
        let text = fs::read_to_string(self.path()).map_err(io_error(self.path()))?;
        let written: CookieJar = text.parse()?;
        // yt-dlp killed while writing the file back leaves it truncated, not signed out
        if written.is_empty() {
            return Ok(0);
        }
        let changes = written.changes_since(&self.base);
        if changes.is_empty() {
            return Ok(0);
        }
        let _lock = self.store.lock()?;
        let mut meta = self.store.read_meta()?;
        let Some(mut jar) = self
            .store
            .read_jar()?
            .filter(|_| meta.generation == self.generation)
        else {
            // the session was replaced or removed meanwhile
            return Ok(0);
        };
        jar.apply(&changes);
        self.store
            .write_atomically(COOKIES_FILE, jar.to_string().as_bytes())?;
        // YouTube clears the login cookies when it ends a session
        if !jar.has_youtube_login() && meta.state.usable() {
            meta.state = SessionState::Expired;
            self.store.write_meta(&meta)?;
            self.store.changed(SessionState::Expired);
            tracing::warn!("YouTube ended the session; import fresh cookies");
        }
        Ok(changes.len())
    }
}

impl Drop for SessionCopy {
    fn drop(&mut self) {
        match self.merge_back() {
            Ok(0) => {}
            Ok(changed) => tracing::debug!(changed, "merged rotated YouTube cookies"),
            Err(e) => tracing::warn!(
                error = %entroximer_core::error::display_chain(&e),
                "cannot merge rotated YouTube cookies"
            ),
        }
    }
}

impl std::fmt::Debug for SessionCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCopy")
            .field("path", &self.path())
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> SessionError {
    let path = path.to_owned();
    move |source| SessionError::Io { path, source }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// FNV-1a: a stable fingerprint for change detection (not for security).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::youtube::cookies::tests::SIGNED_IN;

    fn store() -> (tempfile::TempDir, Arc<SessionStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SessionStore::new(dir.path().join("youtube")));
        (dir, store)
    }

    #[test]
    fn import_keeps_youtube_cookies_privately() {
        let (_dir, store) = store();
        let report = store.import(SIGNED_IN).unwrap();
        assert_eq!(
            report,
            ImportReport {
                cookies: 5,
                dropped: 1
            }
        );

        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Unverified);
        assert_eq!(status.expires_at, Some(1_820_000_000));

        let mode = fs::metadata(store.path(COOKIES_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(matches!(
            store.import("# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tx\n"),
            Err(SessionError::NotSignedIn)
        ));
    }

    #[test]
    fn rotations_merge_back_but_not_into_a_replaced_session() {
        let (dir, store) = store();
        store.import(SIGNED_IN).unwrap();

        // yt-dlp rotates a cookie in its copy
        let copy = store.checkout(dir.path()).unwrap().unwrap();
        let rotated = fs::read_to_string(copy.path())
            .unwrap()
            .replace("psid-secret", "rotated");
        fs::write(copy.path(), rotated).unwrap();
        drop(copy);
        assert!(
            fs::read_to_string(store.path(COOKIES_FILE))
                .unwrap()
                .contains("rotated")
        );

        // a copy of the old session must not leak into a newly imported one
        let stale = store.checkout(dir.path()).unwrap().unwrap();
        fs::write(
            stale.path(),
            fs::read_to_string(stale.path())
                .unwrap()
                .replace("rotated", "stale"),
        )
        .unwrap();
        store.import(SIGNED_IN).unwrap();
        drop(stale);
        let stored = fs::read_to_string(store.path(COOKIES_FILE)).unwrap();
        assert!(!stored.contains("stale"));
    }

    #[test]
    fn later_copies_carry_the_rotated_cookies() {
        let (dir, store) = store();
        store.import(SIGNED_IN).unwrap();
        let copy = store.checkout(dir.path()).unwrap().unwrap();
        let session = copy.session();
        let rotated = fs::read_to_string(copy.path())
            .unwrap()
            .replace("psid-secret", "rotated");
        fs::write(copy.path(), rotated).unwrap();
        drop(copy);

        // even an expired session's cookies go to the downloads of tracks resolved with it
        store.record(1, SessionState::Expired, true).unwrap();
        let again = session.checkout(dir.path()).unwrap().unwrap();
        let text = fs::read_to_string(again.path()).unwrap();
        assert!(text.contains("rotated") && !text.contains("psid-secret"));
        drop(again);

        store.import(SIGNED_IN).unwrap();
        assert!(
            session.checkout(dir.path()).unwrap().is_none(),
            "a replaced session is not handed out"
        );
    }

    #[test]
    fn truncated_write_backs_are_ignored() {
        let (dir, store) = store();
        store.import(SIGNED_IN).unwrap();
        let copy = store.checkout(dir.path()).unwrap().unwrap();
        fs::write(copy.path(), "").unwrap();
        drop(copy);
        assert_eq!(store.status().unwrap().state, SessionState::Unverified);
        assert!(
            fs::read_to_string(store.path(COOKIES_FILE))
                .unwrap()
                .contains("LOGIN_INFO")
        );
    }

    #[test]
    fn cleared_login_cookies_expire_the_session() {
        let (dir, store) = store();
        store.import(SIGNED_IN).unwrap();
        let copy = store.checkout(dir.path()).unwrap().unwrap();
        let signed_out: String = fs::read_to_string(copy.path())
            .unwrap()
            .lines()
            .filter(|l| !l.contains("LOGIN_INFO"))
            .map(|l| format!("{l}\n"))
            .collect();
        fs::write(copy.path(), signed_out).unwrap();
        drop(copy);
        assert_eq!(store.status().unwrap().state, SessionState::Expired);
        assert!(
            store.checkout(dir.path()).unwrap().is_none(),
            "expired sessions are not used"
        );
    }

    #[test]
    fn secrets_are_reimported_only_when_they_change() {
        let (dir, store) = store();
        let secret = dir.path().join("secret.txt");
        fs::write(&secret, SIGNED_IN).unwrap();
        assert!(store.import_file_if_changed(&secret).unwrap().is_some());
        assert!(store.import_file_if_changed(&secret).unwrap().is_none());

        // a manual import is not overwritten by the unchanged secret on the next start
        store
            .import(&SIGNED_IN.replace("login-secret", "manual"))
            .unwrap();
        assert!(store.import_file_if_changed(&secret).unwrap().is_none());

        fs::write(&secret, SIGNED_IN.replace("login-secret", "new-secret")).unwrap();
        assert!(store.import_file_if_changed(&secret).unwrap().is_some());
    }

    #[test]
    fn browser_rotations_merge_only_what_the_browser_changed() {
        let (dir, store) = store();
        store.import(SIGNED_IN).unwrap();
        let stored = || fs::read_to_string(store.path(COOKIES_FILE)).unwrap();

        // a copy dropped untouched (a refused rotation) leaves the session as it was
        let before_refusal = stored();
        drop(store.checkout_any(dir.path()).unwrap().unwrap());
        assert_eq!(stored(), before_refusal);

        let copy = store.checkout_any(dir.path()).unwrap().unwrap();
        // the browser would not take NID, rotated a token, and set cookies of its own
        let taken: CookieJar = SIGNED_IN
            .lines()
            .filter(|l| !l.contains("\tNID\t"))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .parse()
            .unwrap();
        let after: CookieJar = format!(
            "{}.youtube.com\tTRUE\t/\tTRUE\t1830000000\tYSC\tnew\n\
             .example.com\tTRUE\t/\tTRUE\t1830000000\ttracker\tad\n",
            taken.to_string().replace("psid-secret", "rotated")
        )
        .parse()
        .unwrap();
        copy.absorb(&taken, &after).unwrap();
        drop(copy);

        let merged = stored();
        assert!(merged.contains("rotated") && !merged.contains("psid-secret"));
        assert!(merged.contains("\tYSC\tnew"));
        assert!(
            merged.contains("\tNID\t"),
            "cookies the browser did not take are kept"
        );
        assert!(!merged.contains("example.com"), "other sites stay out");
    }

    #[test]
    fn metadata_from_before_rotation_still_reads() {
        let (_dir, store) = store();
        store.import(SIGNED_IN).unwrap();
        fs::write(
            store.path(META_FILE),
            r#"{"generation": 4, "state": "valid", "importedAt": 1, "checkedAt": 2}"#,
        )
        .unwrap();
        let rotation = store.rotation().unwrap();
        assert_eq!(
            rotation,
            RotationState {
                generation: 4,
                state: SessionState::Valid,
                rotated_at: None,
                refusals: 0,
            }
        );
        assert_eq!(store.refused(4).unwrap(), Some(1));
        assert_eq!(
            store.refused(3).unwrap(),
            None,
            "an older session's refusal"
        );
        assert_eq!(store.rotated(4).unwrap(), Some(SessionState::Valid));
        let rotation = store.rotation().unwrap();
        assert_eq!(rotation.refusals, 0);
        assert!(rotation.rotated_at.is_some());

        store.import(SIGNED_IN).unwrap();
        let fresh = store.rotation().unwrap();
        assert_eq!((fresh.rotated_at, fresh.refusals), (None, 0));
    }

    #[test]
    fn changes_are_broadcast() {
        let (dir, store) = store();
        let mut changes = store.subscribe();
        store.import(SIGNED_IN).unwrap();
        assert_eq!(changes.try_recv().unwrap(), SessionState::Unverified);

        // YouTube signing the session out during a yt-dlp run
        let copy = store.checkout(dir.path()).unwrap().unwrap();
        let signed_out: String = fs::read_to_string(copy.path())
            .unwrap()
            .lines()
            .filter(|l| !l.contains("LOGIN_INFO"))
            .map(|l| format!("{l}\n"))
            .collect();
        fs::write(copy.path(), signed_out).unwrap();
        drop(copy);
        assert_eq!(changes.try_recv().unwrap(), SessionState::Expired);

        store.remove().unwrap();
        assert_eq!(changes.try_recv().unwrap(), SessionState::None);
        assert!(changes.try_recv().is_err());
    }

    #[test]
    fn one_rotation_at_a_time() {
        let (_dir, store) = store();
        let held = store.try_lock_rotation().unwrap();
        assert!(held.is_some());
        assert!(store.try_lock_rotation().unwrap().is_none());
        drop(held);
        assert!(store.try_lock_rotation().unwrap().is_some());
    }

    #[test]
    fn state_updates_ignore_replaced_sessions() {
        let (_dir, store) = store();
        store.import(SIGNED_IN).unwrap();
        let old_generation = 1;
        assert_eq!(
            store
                .record(old_generation, SessionState::Valid, true)
                .unwrap(),
            Some(SessionState::Unverified)
        );
        store.import(SIGNED_IN).unwrap();
        assert_eq!(
            store
                .record(old_generation, SessionState::Expired, true)
                .unwrap(),
            None
        );
        assert_eq!(store.status().unwrap().state, SessionState::Unverified);

        assert!(store.remove().unwrap());
        assert_eq!(store.status().unwrap().state, SessionState::None);
    }
}
