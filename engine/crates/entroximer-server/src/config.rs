//! Settings shared by every command. Each can be given as a flag or an `ENTROXIMER_*`
//! environment variable, which is how containers are configured.

use std::convert::Infallible;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::builder::BoolishValueParser;
use clap::{Args, ValueEnum};
use entroximer_core::player::PlayerConfig;
use entroximer_core::timeline::{CacheBudget, RetentionConfig, SpillConfig};
use tokio::sync::Semaphore;

use crate::cache::CacheDir;
use crate::managed_ytdlp::{ManagedYtDlp, YtDlpBinary};
use crate::sources::Sources;
use crate::sources::artwork::Artwork;
use crate::sources::spotify::DEFAULT_MIRROR_SEARCHES;
use crate::uploads::{UploadLimits, UploadStore};
use crate::youtube::{AuthPolicy, YouTube};

const MIB: u64 = 1 << 20;

#[derive(Clone, Debug, Args)]
pub struct MediaConfig {
    #[command(flatten)]
    pub ytdlp: YtDlpConfig,

    #[command(flatten)]
    pub youtube: YouTubeConfig,

    #[command(flatten)]
    pub sources: SourcesConfig,

    #[command(flatten)]
    pub spotify: SpotifyConfig,

    #[command(flatten)]
    pub uploads: UploadsConfig,

    /// Persistent state such as the managed yt-dlp. Defaults to `$XDG_DATA_HOME/entroximer`.
    #[arg(long, env = "ENTROXIMER_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// ffmpeg executable.
    #[arg(long, env = "ENTROXIMER_FFMPEG", default_value = "ffmpeg")]
    pub ffmpeg: String,

    /// ffprobe executable, which reads the tags of direct links and local files.
    #[arg(long, env = "ENTROXIMER_FFPROBE", default_value = "ffprobe")]
    pub ffprobe: String,

    /// Directory for the packet cache and in-progress downloads. Put it on fast disk, never on
    /// a RAM-backed tmpfs. Defaults to `entroximer` in the system temp directory.
    #[arg(long, env = "ENTROXIMER_CACHE_DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Keep the packet cache in RAM only instead of spilling it to `cache_dir`.
    #[arg(long, env = "ENTROXIMER_NO_SPILL", value_parser = BoolishValueParser::new())]
    pub no_spill: bool,

    /// Seconds of audio behind the playhead kept in RAM.
    #[arg(long, env = "ENTROXIMER_MEMORY_BEHIND_SECS", default_value_t = 15)]
    pub memory_behind_secs: u64,

    /// How much more is kept ahead of the playhead than behind it, in RAM and on disk.
    #[arg(long, env = "ENTROXIMER_FORWARD_BIAS", default_value_t = 4.0)]
    pub forward_bias: f64,

    /// RAM cap for one track's cached audio, in MiB.
    #[arg(long, env = "ENTROXIMER_MEMORY_MAX_MB", default_value_t = 8)]
    pub memory_max_mb: u64,

    /// Disk cap for one track's cached audio, in MiB.
    #[arg(long, env = "ENTROXIMER_DISK_MAX_MB", default_value_t = 1024)]
    pub disk_max_mb: u64,

    /// RAM cap for all tracks' cached audio together, in MiB. Each track playing, preparing or
    /// sounding as a clip may take an equal share of it, up to `memory_max_mb`.
    #[arg(long, env = "ENTROXIMER_MEMORY_TOTAL_MB", default_value_t = 128)]
    pub memory_total_mb: u64,

    /// Disk cap for all tracks' cached audio together, in MiB, shared like `memory_total_mb`.
    #[arg(long, env = "ENTROXIMER_DISK_TOTAL_MB", default_value_t = 4096)]
    pub disk_total_mb: u64,
}

#[derive(Clone, Debug, Args)]
pub struct YtDlpConfig {
    /// Let the engine install yt-dlp into the data directory and keep it updated, instead of
    /// running `--ytdlp`. Lets yt-dlp be updated without rebuilding the container image.
    #[arg(long = "ytdlp-managed", env = "ENTROXIMER_YTDLP_MANAGED", value_parser = BoolishValueParser::new())]
    pub managed: bool,

    /// yt-dlp executable when not managed.
    #[arg(long = "ytdlp", env = "ENTROXIMER_YTDLP", default_value = "yt-dlp")]
    pub program: String,

    /// What the managed install contains, as a pip requirement: pin with
    /// `yt-dlp[default]==<version>`, or use `yt-dlp[default] @ git+https://...` for a test build.
    #[arg(
        long = "ytdlp-package",
        env = "ENTROXIMER_YTDLP_PACKAGE",
        default_value = "yt-dlp[default]"
    )]
    pub package: String,

    /// Install yt-dlp's nightly builds, which get YouTube fixes first.
    #[arg(long = "ytdlp-nightly", env = "ENTROXIMER_YTDLP_NIGHTLY", value_parser = BoolishValueParser::new())]
    pub nightly: bool,

    /// Hours between checks for a newer managed yt-dlp while serving; 0 disables them.
    #[arg(
        long = "ytdlp-update-hours",
        env = "ENTROXIMER_YTDLP_UPDATE_HOURS",
        default_value_t = 24
    )]
    pub update_hours: u64,

    /// Python used to create the managed install.
    #[arg(long, env = "ENTROXIMER_PYTHON", default_value = "python3")]
    pub python: String,

    /// Extra arguments for every yt-dlp run, whitespace separated
    /// (e.g. `--extractor-args youtube:formats=duplicate`).
    #[arg(long = "ytdlp-args", env = "ENTROXIMER_YTDLP_ARGS", default_value = "")]
    pub args: ExtraArgs,

    /// yt-dlp runs (searches, and loading tracks) at once, counted separately for searches so
    /// that searching as users type never holds up playback. Each run is a Python process,
    /// with a JavaScript runtime for YouTube, that peaks at 50 to 350 MB. Downloads do not count.
    #[arg(
        long = "ytdlp-max-runs",
        env = "ENTROXIMER_YTDLP_MAX_RUNS",
        default_value_t = 2
    )]
    pub max_runs: usize,
}

#[derive(Clone, Debug, Args)]
pub struct SourcesConfig {
    /// Search for queries that are neither links nor `prefix:terms`, as a search prefix.
    #[arg(
        long = "default-search",
        env = "ENTROXIMER_DEFAULT_SEARCH",
        default_value = "ytsearch"
    )]
    pub default_search: String,

    /// Most tracks loaded from one playlist.
    #[arg(
        long = "playlist-limit",
        env = "ENTROXIMER_PLAYLIST_LIMIT",
        default_value_t = 500
    )]
    pub playlist_limit: u32,

    /// Let clients play any file on the engine's host that it can read, by its path.
    #[arg(long = "allow-local-files", env = "ENTROXIMER_ALLOW_LOCAL_FILES", value_parser = BoolishValueParser::new())]
    pub allow_local_files: bool,

    /// Let clients play the files within this directory by their path (relative paths are
    /// within it), and no others, e.g. a bot's sounds on a volume it shares with the engine.
    #[arg(long = "local-files-dir", env = "ENTROXIMER_LOCAL_FILES_DIR")]
    pub local_files_dir: Option<PathBuf>,

    /// Let clients make the engine fetch links to this machine or private networks, e.g. a
    /// media server on the LAN.
    #[arg(long = "allow-private-urls", env = "ENTROXIMER_ALLOW_PRIVATE_URLS", value_parser = BoolishValueParser::new())]
    pub allow_private_urls: bool,
}

#[derive(Clone, Debug, Args)]
pub struct SpotifyConfig {
    /// Client ID of a Spotify app (developer.spotify.com), which enables Spotify links and
    /// `spsearch:`. Spotify only provides the metadata; tracks play from YouTube.
    #[arg(long = "spotify-client-id", env = "ENTROXIMER_SPOTIFY_CLIENT_ID")]
    pub client_id: Option<String>,

    /// Client secret of that Spotify app.
    #[arg(
        long = "spotify-client-secret",
        env = "ENTROXIMER_SPOTIFY_CLIENT_SECRET",
        hide_env_values = true
    )]
    pub client_secret: Option<Secret>,

    /// Country whose Spotify catalog is read, as a two-letter code.
    #[arg(
        long = "spotify-market",
        env = "ENTROXIMER_SPOTIFY_MARKET",
        default_value = "US"
    )]
    pub market: String,

    /// Searches that find a Spotify track on YouTube, tried in order, comma separated. `{isrc}`,
    /// `{title}`, `{artist}` (the first) and `{artists}` stand for the track's; searches using
    /// `{isrc}` are skipped for tracks without one.
    #[arg(
        long = "spotify-mirror",
        env = "ENTROXIMER_SPOTIFY_MIRROR",
        value_delimiter = ',',
        default_values = DEFAULT_MIRROR_SEARCHES
    )]
    pub mirror: Vec<String>,
}

#[derive(Clone, Debug, Args)]
pub struct UploadsConfig {
    /// Let clients upload audio files to play (`POST /v1/uploads`), e.g. sounds that have no
    /// URL. Anyone who may use the engine can then store files on it, within the limits below.
    #[arg(long = "uploads", env = "ENTROXIMER_UPLOADS", value_parser = BoolishValueParser::new())]
    pub enabled: bool,

    /// The largest upload, in MiB.
    #[arg(
        long = "upload-max-mb",
        env = "ENTROXIMER_UPLOAD_MAX_MB",
        default_value_t = 50
    )]
    pub max_mb: u64,

    /// Disk all uploads may take together, in MiB. When a new one needs room, expired uploads
    /// go first, then those unused for longest; ones playing are kept.
    #[arg(
        long = "upload-total-mb",
        env = "ENTROXIMER_UPLOAD_TOTAL_MB",
        default_value_t = 512
    )]
    pub total_mb: u64,

    /// Minutes an upload is kept after it was uploaded or last used.
    #[arg(
        long = "upload-keep-mins",
        env = "ENTROXIMER_UPLOAD_KEEP_MINS",
        default_value_t = 60
    )]
    pub keep_mins: u64,
}

impl UploadsConfig {
    pub fn limits(&self) -> UploadLimits {
        let total_bytes = self.total_mb * MIB;
        UploadLimits {
            max_bytes: (self.max_mb * MIB).min(total_bytes),
            total_bytes,
            keep: Duration::from_secs(self.keep_mins * 60),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct YouTubeConfig {
    /// When the YouTube session is sent: only for videos that need it (age-restricted, bot
    /// checks, members-only), which keeps the account out of most traffic, always, or never.
    #[arg(
        long = "youtube-auth",
        env = "ENTROXIMER_YOUTUBE_AUTH",
        value_enum,
        default_value_t
    )]
    pub auth: AuthPolicy,

    /// A cookies.txt imported whenever its contents change, e.g. a mounted Podman/Docker secret.
    #[arg(long = "youtube-cookies-file", env = "ENTROXIMER_YOUTUBE_COOKIES_FILE")]
    pub cookies_file: Option<PathBuf>,

    /// Hours between checks that YouTube still accepts the session; 0 disables them.
    #[arg(
        long = "youtube-check-hours",
        env = "ENTROXIMER_YOUTUBE_CHECK_HOURS",
        default_value_t = 12
    )]
    pub check_hours: u64,

    /// An age-restricted video that only loads when signed in, used for those checks.
    #[arg(
        long = "youtube-check-video",
        env = "ENTROXIMER_YOUTUBE_CHECK_VIDEO",
        default_value = "https://www.youtube.com/watch?v=Tq92D6wQ1mg"
    )]
    pub check_video: String,
}

impl MediaConfig {
    /// Where persistent state lives: `data_dir`, else the XDG data directory.
    pub fn data_root(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::home_dir().map(|home| home.join(".local").join("share")))
                .unwrap_or_else(std::env::temp_dir)
                .join("entroximer")
        })
    }

    /// The managed yt-dlp install, whether or not it is enabled.
    pub fn managed_ytdlp(&self) -> ManagedYtDlp {
        ManagedYtDlp::new(
            self.data_root().join("yt-dlp"),
            self.ytdlp.python.clone(),
            self.ytdlp.package.clone(),
            self.ytdlp.nightly,
        )
    }

    /// Where run directories are created: `cache_dir`, or `entroximer` in the temp directory.
    pub fn cache_root(&self) -> PathBuf {
        self.cache_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("entroximer"))
    }
}

/// Settings together with this process's cache directory; everything that loads media needs both.
#[derive(Debug)]
pub struct MediaContext {
    pub config: MediaConfig,
    pub ytdlp: YtDlpBinary,
    pub ytdlp_runs: YtDlpRuns,
    pub youtube: YouTube,
    pub sources: Sources,
    /// Artwork the engine finds itself: pages' preview pictures and files' embedded covers.
    pub artwork: Artwork,
    /// Files clients uploaded, if uploads are on.
    pub uploads: Option<Arc<UploadStore>>,
    cache: CacheDir,
    /// What all tracks' cached audio may take together.
    cache_budget: Arc<CacheBudget>,
}

/// Turns for yt-dlp runs; see [`YtDlpConfig::max_runs`].
#[derive(Debug)]
pub struct YtDlpRuns {
    pub searches: Semaphore,
    /// Loading links, resolving tracks to play, and checking the YouTube session.
    pub extractions: Semaphore,
}

impl MediaContext {
    pub fn new(config: MediaConfig) -> anyhow::Result<Self> {
        let cache = CacheDir::acquire(&config.cache_root())?;
        let ytdlp = if config.ytdlp.managed {
            YtDlpBinary::Managed(Arc::new(config.managed_ytdlp()))
        } else {
            YtDlpBinary::System(config.ytdlp.program.clone())
        };
        let runs = config.ytdlp.max_runs.max(1);
        let ytdlp_runs = YtDlpRuns {
            searches: Semaphore::new(runs),
            extractions: Semaphore::new(runs),
        };
        let youtube = YouTube::new(
            config.data_root().join("youtube"),
            config.youtube.auth,
            config.youtube.check_video.clone(),
        );
        let uploads = if config.uploads.enabled {
            let dir = cache.path().join("uploads");
            let store = UploadStore::new(dir.clone(), config.uploads.limits())
                .with_context(|| format!("cannot create {}", dir.display()))?;
            Some(Arc::new(store))
        } else {
            None
        };
        let sources = Sources::new(&config.sources, &config.spotify, uploads.clone())?;
        let cache_budget = Arc::new(CacheBudget::new(
            usize::try_from(config.memory_total_mb * MIB).unwrap_or(usize::MAX),
            config.disk_total_mb * MIB,
        ));
        Ok(Self {
            config,
            ytdlp,
            ytdlp_runs,
            youtube,
            sources,
            artwork: Artwork::default(),
            uploads,
            cache,
            cache_budget,
        })
    }

    /// Directory for spilled packets and in-progress downloads.
    pub fn work_dir(&self) -> &Path {
        self.cache.path()
    }

    pub fn player_config(&self) -> PlayerConfig {
        let c = &self.config;
        let spill = (!c.no_spill).then(|| SpillConfig {
            dir: self.work_dir().to_owned(),
            max_bytes: c.disk_max_mb * MIB,
            ..SpillConfig::default()
        });
        PlayerConfig {
            retention: RetentionConfig {
                memory_behind: Duration::from_secs(c.memory_behind_secs),
                forward_bias: c.forward_bias,
                memory_max_bytes: usize::try_from(c.memory_max_mb * MIB).unwrap_or(usize::MAX),
                spill,
                budget: Some(Arc::clone(&self.cache_budget)),
            },
            ..PlayerConfig::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Pretty,
    /// One JSON object per line, for log collectors.
    Json,
}

/// A setting that must not show up in logs, such as a password: its `Debug` output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether it is blank, as compose files pass unset variables.
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl FromStr for Secret {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Whitespace-separated command-line arguments for another program.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtraArgs(pub Vec<String>);

impl FromStr for ExtraArgs {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.split_whitespace().map(str::to_owned).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let secret: Secret = "hunter2".parse().unwrap();
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(format!("{:?}", Some(secret.clone())), "Some(<redacted>)");
        assert_eq!(secret.expose(), "hunter2");
        assert!(Secret::new("  ").is_blank());
    }
}
