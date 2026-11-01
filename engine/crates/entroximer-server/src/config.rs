//! Settings shared by every command. Each can be given as a flag or an `ENTROXIMER_*`
//! environment variable, which is how containers are configured.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::builder::BoolishValueParser;
use clap::{Args, ValueEnum};
use entroximer_core::player::PlayerConfig;
use entroximer_core::timeline::{RetentionConfig, SpillConfig};

use crate::cache::CacheDir;
use crate::managed_ytdlp::{ManagedYtDlp, YtDlpBinary};
use crate::youtube::{AuthPolicy, YouTube};

const MIB: u64 = 1 << 20;

#[derive(Clone, Debug, Args)]
pub struct MediaConfig {
    #[command(flatten)]
    pub ytdlp: YtDlpConfig,

    #[command(flatten)]
    pub youtube: YouTubeConfig,

    /// Persistent state such as the managed yt-dlp. Defaults to `$XDG_DATA_HOME/entroximer`.
    #[arg(long, env = "ENTROXIMER_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// ffmpeg executable.
    #[arg(long, env = "ENTROXIMER_FFMPEG", default_value = "ffmpeg")]
    pub ffmpeg: String,

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
    pub youtube: YouTube,
    cache: CacheDir,
}

impl MediaContext {
    pub fn new(config: MediaConfig) -> anyhow::Result<Self> {
        let cache = CacheDir::acquire(&config.cache_root())?;
        let ytdlp = if config.ytdlp.managed {
            YtDlpBinary::Managed(Arc::new(config.managed_ytdlp()))
        } else {
            YtDlpBinary::System(config.ytdlp.program.clone())
        };
        let youtube = YouTube::new(
            config.data_root().join("youtube"),
            config.youtube.auth,
            config.youtube.check_video.clone(),
        );
        Ok(Self {
            config,
            ytdlp,
            youtube,
            cache,
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

/// Whitespace-separated command-line arguments for another program.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtraArgs(pub Vec<String>);

impl FromStr for ExtraArgs {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.split_whitespace().map(str::to_owned).collect()))
    }
}
