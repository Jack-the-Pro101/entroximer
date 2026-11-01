//! yt-dlp installed and kept up to date by the engine itself, independent of the container image.
//!
//! YouTube changes break yt-dlp regularly and fixes ship within days, too often to rebuild and
//! redeploy images for. The engine therefore keeps yt-dlp in Python virtual environments under
//! its data directory:
//!
//! - every install lives in its own `venv-*` directory and is complete and verified before use;
//! - `current` is a symlink to the active install, replaced atomically, so a yt-dlp started at
//!   any moment runs either the old or the new version, never a half-written one;
//! - the previous install is kept until the next update, so downloads already running finish;
//! - an advisory lock serialises updates between a running server and `entroximer ytdlp update`.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::watch;

const CURRENT: &str = "current";
const INSTALL_PREFIX: &str = "venv-";
const LOCK_FILE: &str = "update.lock";

/// After a failed install or update check, try again this soon rather than a full interval later.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("cannot prepare {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot run `{program}`")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("`{command}` failed: {message}")]
    Command { command: String, message: String },
    #[error("pip returned an unexpected report")]
    Report(#[source] serde_json::Error),
    #[error("yt-dlp is not installed yet; the engine installs it on startup")]
    NotInstalled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    Installed { version: String },
    Updated { from: String, to: String },
    UpToDate { version: String },
}

#[derive(Debug)]
pub struct ManagedYtDlp {
    root: PathBuf,
    python: String,
    /// A pip requirement: `yt-dlp[default]`, a pinned version, or a git URL for a test build.
    package: String,
    /// Allow pre-releases, i.e. yt-dlp's nightly builds.
    pre_release: bool,
    /// Version of the active install, once known.
    installed: watch::Sender<Option<String>>,
}

impl ManagedYtDlp {
    pub fn new(root: PathBuf, python: String, package: String, pre_release: bool) -> Self {
        Self {
            root,
            python,
            package,
            pre_release,
            installed: watch::Sender::new(None),
        }
    }

    /// The yt-dlp to run. The path goes through the `current` symlink, so every new process
    /// picks up the latest install.
    pub fn executable(&self) -> PathBuf {
        self.root.join(CURRENT).join("bin").join("yt-dlp")
    }

    /// Version of the active install, by asking it.
    pub async fn installed_version(&self) -> Option<String> {
        let executable = self.executable();
        if !executable.exists() {
            return None;
        }
        let version = run(Command::new(&executable).arg("--version"))
            .await
            .ok()
            .map(|v| v.trim().to_owned());
        self.installed.send_replace(version.clone());
        version
    }

    /// Waits up to `timeout` for an install, e.g. while the server installs on first start.
    pub async fn wait_until_installed(&self, timeout: Duration) -> Result<(), InstallError> {
        if self.executable().exists() {
            return Ok(());
        }
        let mut installed = self.installed.subscribe();
        tokio::time::timeout(timeout, installed.wait_for(Option::is_some))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|_| ())
            .ok_or(InstallError::NotInstalled)
    }

    /// Installs yt-dlp if missing, or replaces the install when pip finds anything newer.
    pub async fn update(&self) -> Result<UpdateOutcome, InstallError> {
        fs::create_dir_all(&self.root).map_err(|source| InstallError::Io {
            path: self.root.clone(),
            source,
        })?;
        let _lock = self.lock().await?;

        let current = self.installed_version().await;
        if let Some(version) = &current
            && !self.update_available().await?
        {
            return Ok(UpdateOutcome::UpToDate {
                version: version.clone(),
            });
        }

        let previous = fs::read_link(self.root.join(CURRENT)).ok();
        let fresh = self.build_install().await?;
        let version = run(Command::new(fresh.join("bin").join("yt-dlp")).arg("--version"))
            .await?
            .trim()
            .to_owned();
        self.activate(&fresh)?;
        self.prune(&fresh, previous.as_deref());
        self.installed.send_replace(Some(version.clone()));

        Ok(match current {
            None => UpdateOutcome::Installed { version },
            Some(from) => UpdateOutcome::Updated { from, to: version },
        })
    }

    /// Installs yt-dlp if missing and, given an interval, checks for updates that often.
    /// Failures are retried after a short delay. Returns only when there is nothing left to do.
    pub async fn maintain(&self, update_interval: Option<Duration>) {
        loop {
            if update_interval.is_none() && self.installed_version().await.is_some() {
                return;
            }
            match self.update().await {
                Ok(outcome) => {
                    log_outcome(&outcome);
                    match update_interval {
                        Some(interval) => tokio::time::sleep(interval).await,
                        None => return,
                    }
                }
                Err(e) => {
                    let error = entroximer_core::error::display_chain(&e);
                    tracing::warn!(retry_in = ?RETRY_AFTER_FAILURE, %error, "yt-dlp install or update failed");
                    tokio::time::sleep(RETRY_AFTER_FAILURE).await;
                }
            }
        }
    }

    /// Asks pip whether installing the requirement now would change anything.
    async fn update_available(&self) -> Result<bool, InstallError> {
        let pip = self.root.join(CURRENT).join("bin").join("pip");
        let report = run(Command::new(pip)
            .args(["install", "--dry-run", "--quiet", "--report", "-"])
            .args(["--upgrade", "--upgrade-strategy", "eager"])
            .args(self.pip_flags())
            .arg(&self.package))
        .await?;
        Ok(!parse_report(&report)?.install.is_empty())
    }

    async fn build_install(&self) -> Result<PathBuf, InstallError> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let dir = self.root.join(format!("{INSTALL_PREFIX}{stamp}"));
        run(Command::new(&self.python).args([OsStr::new("-m"), "venv".as_ref(), dir.as_os_str()]))
            .await?;
        let installed = run(Command::new(dir.join("bin").join("pip"))
            .args(["install", "--quiet", "--no-cache-dir"])
            .args(self.pip_flags())
            .arg(&self.package))
        .await;
        if let Err(e) = installed {
            // never leave a broken install behind
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok(dir)
    }

    fn pip_flags(&self) -> Vec<&'static str> {
        let mut flags = vec!["--disable-pip-version-check"];
        if self.pre_release {
            flags.push("--pre");
        }
        flags
    }

    /// Points `current` at `install` with an atomic rename of a fresh symlink.
    fn activate(&self, install: &Path) -> Result<(), InstallError> {
        let target = install.file_name().expect("installs are named directories");
        let staged = self.root.join(format!("{CURRENT}.new"));
        let io_error = |source| InstallError::Io {
            path: staged.clone(),
            source,
        };
        // left over if a previous update was interrupted
        if let Err(e) = fs::remove_file(&staged)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(io_error(e));
        }
        std::os::unix::fs::symlink(target, &staged).map_err(io_error)?;
        fs::rename(&staged, self.root.join(CURRENT)).map_err(io_error)
    }

    /// Removes every install except the active one and the one it replaced.
    fn prune(&self, active: &Path, previous: Option<&Path>) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        let names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let keep = [active.file_name(), previous.and_then(Path::file_name)];
        for name in stale_installs(&names, &keep) {
            let path = self.root.join(name);
            if let Err(e) = fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), "cannot remove old yt-dlp install: {e}");
            }
        }
    }

    async fn lock(&self) -> Result<File, InstallError> {
        let path = self.root.join(LOCK_FILE);
        let file = File::create(&path).map_err(|source| InstallError::Io {
            path: path.clone(),
            source,
        })?;
        tokio::task::spawn_blocking(move || file.lock().map(|()| file))
            .await
            .expect("locking does not panic")
            .map_err(|source| InstallError::Io { path, source })
    }
}

pub fn log_outcome(outcome: &UpdateOutcome) {
    match outcome {
        UpdateOutcome::Installed { version } => tracing::info!(%version, "installed yt-dlp"),
        UpdateOutcome::Updated { from, to } => tracing::info!(%from, %to, "updated yt-dlp"),
        UpdateOutcome::UpToDate { version } => tracing::info!(%version, "yt-dlp is up to date"),
    }
}

#[derive(Debug, Deserialize)]
struct PipReport {
    #[serde(default)]
    install: Vec<serde_json::Value>,
}

fn parse_report(json: &str) -> Result<PipReport, InstallError> {
    serde_json::from_str(json).map_err(InstallError::Report)
}

fn stale_installs<'a>(names: &'a [String], keep: &[Option<&OsStr>]) -> Vec<&'a str> {
    names
        .iter()
        .map(String::as_str)
        .filter(|name| name.starts_with(INSTALL_PREFIX))
        .filter(|name| !keep.iter().flatten().any(|k| *k == OsStr::new(name)))
        .collect()
}

/// Runs a command to completion and returns its stdout; failures carry the end of stderr.
async fn run(command: &mut Command) -> Result<String, InstallError> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let output = command
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| InstallError::Spawn {
            program: program.clone(),
            source,
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no error output")
        .trim()
        .to_owned();
    let args: Vec<_> = command
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    Err(InstallError::Command {
        command: format!("{program} {}", args.join(" ")),
        message,
    })
}

/// The yt-dlp the engine runs: one found on the system, or the engine-managed install.
#[derive(Clone, Debug)]
pub enum YtDlpBinary {
    System(String),
    Managed(Arc<ManagedYtDlp>),
}

impl YtDlpBinary {
    /// How long resolving waits for a first install still in progress.
    const INSTALL_WAIT: Duration = Duration::from_secs(180);

    pub fn program(&self) -> String {
        match self {
            Self::System(program) => program.clone(),
            Self::Managed(managed) => managed.executable().to_string_lossy().into_owned(),
        }
    }

    /// Succeeds once there is a yt-dlp to run.
    pub async fn ready(&self) -> Result<(), InstallError> {
        match self {
            Self::System(_) => Ok(()),
            Self::Managed(managed) => managed.wait_until_installed(Self::INSTALL_WAIT).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pip_report_tells_whether_anything_changes() {
        let up_to_date =
            r#"{"version": "1", "pip_version": "25.1.1", "install": [], "environment": {}}"#;
        assert!(parse_report(up_to_date).unwrap().install.is_empty());
        let newer = r#"{"version": "1", "install": [{"metadata": {"name": "yt-dlp", "version": "2026.10.29"}}]}"#;
        assert_eq!(parse_report(newer).unwrap().install.len(), 1);
        assert!(parse_report("not json").is_err());
    }

    #[test]
    fn keeps_only_active_and_previous_installs() {
        let names: Vec<String> = [
            "venv-1",
            "venv-2",
            "venv-3",
            "current",
            "update.lock",
            "venv-4",
        ]
        .map(String::from)
        .into();
        let stale = stale_installs(
            &names,
            &[Some(OsStr::new("venv-4")), Some(OsStr::new("venv-3"))],
        );
        assert_eq!(stale, ["venv-1", "venv-2"]);
        let first_install = stale_installs(&names, &[Some(OsStr::new("venv-4")), None]);
        assert_eq!(first_install, ["venv-1", "venv-2", "venv-3"]);
    }

    #[tokio::test]
    async fn failed_commands_report_their_error() {
        let err = run(Command::new("sh").args([
            "-c",
            "echo first >&2; echo 'No matching distribution' >&2; exit 1",
        ]))
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            InstallError::Command { ref message, .. } if message == "No matching distribution"
        ));
    }
}
