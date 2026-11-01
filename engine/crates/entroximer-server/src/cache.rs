//! The per-process working directory for spilled packets and in-progress downloads.
//!
//! Each engine process works in its own `run-*` directory inside the configured cache root and
//! holds an advisory lock on a sibling `run-*.lock` file while running. The directory is deleted
//! on exit; directories left by a crashed process are recognised by their unheld lock and removed
//! by the next process to start, even while other engines share the same root.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;

const RUN_PREFIX: &str = "run-";
const LOCK_SUFFIX: &str = ".lock";

#[derive(Debug)]
pub struct CacheDir {
    dir: tempfile::TempDir,
    lock_path: PathBuf,
    _lock: File,
}

impl CacheDir {
    pub fn acquire(root: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("cannot create cache directory {}", root.display()))?;
        remove_stale_runs(root);

        let dir = tempfile::Builder::new()
            .prefix(RUN_PREFIX)
            .tempdir_in(root)
            .with_context(|| format!("cannot create a run directory in {}", root.display()))?;
        let lock_path = lock_path_for(dir.path());
        let lock = File::create(&lock_path)
            .with_context(|| format!("cannot create {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("cannot lock {}", lock_path.display()))?;
        Ok(Self {
            dir,
            lock_path,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for CacheDir {
    fn drop(&mut self) {
        // the directory itself is removed by `TempDir`
        if let Err(e) = fs::remove_file(&self.lock_path) {
            tracing::warn!(path = %self.lock_path.display(), "cannot remove cache lock: {e}");
        }
    }
}

fn lock_path_for(run_dir: &Path) -> PathBuf {
    let mut name = run_dir.file_name().unwrap_or_default().to_owned();
    name.push(LOCK_SUFFIX);
    run_dir.with_file_name(name)
}

/// Removes run directories whose owning process is gone. Problems are logged, not fatal: a
/// leftover directory only costs disk space.
fn remove_stale_runs(root: &Path) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(root = %root.display(), "cannot scan cache directory: {e}");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(run) = name
            .to_str()
            .filter(|n| n.starts_with(RUN_PREFIX))
            .and_then(|n| n.strip_suffix(LOCK_SUFFIX))
        else {
            continue;
        };
        let lock_path = entry.path();
        if let Err(e) = remove_if_stale(&root.join(run), &lock_path) {
            tracing::warn!(run, "cannot clean up stale cache: {e}");
        }
    }
}

fn remove_if_stale(run_dir: &Path, lock_path: &Path) -> io::Result<()> {
    let lock = File::open(lock_path)?;
    match lock.try_lock() {
        Ok(()) => {}
        // still owned by a running engine
        Err(fs::TryLockError::WouldBlock) => return Ok(()),
        Err(fs::TryLockError::Error(e)) => return Err(e),
    }
    if let Err(e) = fs::remove_dir_all(run_dir)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e);
    }
    tracing::info!(dir = %run_dir.display(), "removed cache left by a stopped engine");
    fs::remove_file(lock_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_runs_are_removed_and_live_ones_kept() {
        let root = tempfile::tempdir().unwrap();

        // a crashed engine: directory and lock file, but nobody holds the lock
        let crashed = root.path().join("run-crashed");
        fs::create_dir(&crashed).unwrap();
        fs::write(crashed.join("partial.webm"), b"...").unwrap();
        File::create(root.path().join("run-crashed.lock")).unwrap();

        let live = CacheDir::acquire(root.path()).unwrap();
        assert!(!crashed.exists());
        assert!(!root.path().join("run-crashed.lock").exists());

        // a second engine sharing the root leaves the first one alone
        let other = CacheDir::acquire(root.path()).unwrap();
        assert!(live.path().exists());
        assert_ne!(live.path(), other.path());

        let live_path = live.path().to_owned();
        drop(live);
        assert!(!live_path.exists());
        assert!(!lock_path_for(&live_path).exists());
        assert!(other.path().exists());
    }
}
