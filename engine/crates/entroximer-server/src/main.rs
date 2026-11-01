//! The Entroximer engine binary.

use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use entroximer_core::error::display_chain;
use entroximer_server::config::{LogFormat, MediaConfig, MediaContext};
use entroximer_server::managed_ytdlp::log_outcome;
use entroximer_server::youtube::command::YouTubeCommand;
use entroximer_server::{render, serve, youtube};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "entroximer", version, about = "Discord audio engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    media: MediaConfig,

    /// Log output format. Verbosity is set with `RUST_LOG`, e.g. `RUST_LOG=entroximer=debug`.
    #[arg(
        long,
        env = "ENTROXIMER_LOG_FORMAT",
        value_enum,
        default_value_t,
        global = true
    )]
    log_format: LogFormat,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the engine server.
    Serve(serve::ServeArgs),
    /// Render a track to a WAV file through the full pipeline.
    Render(render::RenderArgs),
    /// Exit successfully if the local engine is healthy (for container health checks).
    Healthcheck(serve::ListenArgs),
    /// Manage the engine-installed yt-dlp (see `--ytdlp-managed`).
    #[command(subcommand)]
    Ytdlp(YtdlpCommand),
    /// Sign the engine into YouTube, for age-restricted and members-only videos.
    #[command(subcommand)]
    Youtube(YouTubeCommand),
}

#[derive(Debug, Subcommand)]
enum YtdlpCommand {
    /// Install the managed yt-dlp, or replace it if a newer release exists. Safe while the
    /// engine runs: new downloads switch to the new install, running ones finish on the old.
    Update,
    /// Show the managed yt-dlp's version and location.
    Status,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.log_format);
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{}", display_chain(e.as_ref()));
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Healthcheck(args) => serve::check_health(&args).await,
        Command::Serve(args) => serve::run(args, Arc::new(MediaContext::new(cli.media)?)).await,
        Command::Render(args) => {
            // rendering is run by the operator, who may play their own files
            let mut media = cli.media;
            media.sources.allow_local_files = true;
            render::run(args, &MediaContext::new(media)?).await
        }
        Command::Youtube(command) => {
            youtube::command::run(command, &MediaContext::new(cli.media)?).await
        }
        Command::Ytdlp(command) => ytdlp(command, &cli.media).await,
    }
}

async fn ytdlp(command: YtdlpCommand, media: &MediaConfig) -> anyhow::Result<()> {
    let managed = media.managed_ytdlp();
    if !media.ytdlp.managed {
        tracing::warn!(
            "ENTROXIMER_YTDLP_MANAGED is off; the engine runs `{}` instead",
            media.ytdlp.program
        );
    }
    match command {
        YtdlpCommand::Update => log_outcome(&managed.update().await?),
        YtdlpCommand::Status => match managed.installed_version().await {
            Some(version) => println!("yt-dlp {version} at {}", managed.executable().display()),
            None => println!("not installed; run `entroximer ytdlp update`"),
        },
    }
    Ok(())
}

/// Without `RUST_LOG`: songbird reports every player it sets up at `info`, which floods the logs
/// of a busy engine.
const DEFAULT_LOG_FILTER: &str = "info,songbird=warn";

fn init_logging(format: LogFormat) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}
