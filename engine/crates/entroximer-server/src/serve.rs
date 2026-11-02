//! `entroximer serve`: the long-running engine. Serves the client API (see [`crate::api`]),
//! maintains the managed yt-dlp and the YouTube session, and shuts down gracefully on
//! SIGTERM/SIGINT.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::api;
use crate::config::{MediaContext, Secret};
use crate::managed_ytdlp::YtDlpBinary;
use crate::node::{Node, NodeSettings};
use crate::youtube;

pub const HEALTH_PATH: &str = "/health";

#[derive(Clone, Debug, Args)]
pub struct ListenArgs {
    /// Address to listen on; `0.0.0.0` inside containers.
    #[arg(long, env = "ENTROXIMER_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    #[arg(long, env = "ENTROXIMER_PORT", default_value_t = 2333)]
    pub port: u16,
}

#[derive(Clone, Debug, Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub listen: ListenArgs,

    /// Password clients must send as `Authorization`. Without one, any client that can reach
    /// the engine may use it.
    #[arg(long, env = "ENTROXIMER_PASSWORD", hide_env_values = true)]
    pub password: Option<Secret>,

    /// Seconds between `playerUpdate` events.
    #[arg(long, env = "ENTROXIMER_PLAYER_UPDATE_SECS", default_value_t = 5)]
    pub player_update_secs: u64,

    /// A track waiting this long for media is reported stuck.
    #[arg(long, env = "ENTROXIMER_STUCK_MS", default_value_t = 10_000)]
    pub stuck_ms: u64,

    /// How long a disconnected client's players stay paused, waiting for it to resume its
    /// session; afterwards they are destroyed.
    #[arg(long, env = "ENTROXIMER_RESUME_TIMEOUT_SECS", default_value_t = 60)]
    pub resume_timeout_secs: u64,

    /// Seconds before the current track ends at which a player starts loading the next item of
    /// its queue, so that it follows without a gap.
    #[arg(long, env = "ENTROXIMER_PRELOAD_SECS", default_value_t = 30)]
    pub preload_secs: u64,
}

pub async fn run(args: ServeArgs, ctx: Arc<MediaContext>) -> anyhow::Result<()> {
    let addr = SocketAddr::new(args.listen.host, args.listen.port);
    // an empty variable, e.g. from `${ENTROXIMER_PASSWORD:-}` in compose files, means none
    let password = args.password.filter(|p| !p.is_blank());
    if password.is_none() {
        if addr.ip().is_loopback() {
            tracing::info!("no password set; local clients connect without one");
        } else {
            tracing::warn!(
                %addr,
                "no password set: anyone who can reach this address can control the engine and its YouTube session"
            );
        }
    }
    if let YtDlpBinary::Managed(managed) = &ctx.ytdlp {
        let hours = ctx.config.ytdlp.update_hours;
        let interval = (hours > 0).then(|| Duration::from_secs(hours * 3600));
        let managed = Arc::clone(managed);
        tokio::spawn(async move { managed.maintain(interval).await });
    }
    if let Some(uploads) = &ctx.uploads {
        let limits = uploads.limits();
        tracing::info!(
            max_mb = limits.max_bytes >> 20,
            total_mb = limits.total_bytes >> 20,
            keep_mins = limits.keep.as_secs() / 60,
            "uploads are on"
        );
        if password.is_none() && !addr.ip().is_loopback() {
            tracing::warn!(
                %addr,
                "uploads are on without a password: anyone who can reach this address can store files on the engine"
            );
        }
        tokio::spawn(Arc::clone(uploads).maintain());
    }
    start_youtube_session(&ctx);
    tokio::spawn({
        let ctx = Arc::clone(&ctx);
        async move { ctx.sources.report().await }
    });

    let node = Node::new(
        Arc::clone(&ctx),
        NodeSettings {
            password,
            player_update_interval: Duration::from_secs(args.player_update_secs.max(1)),
            stuck_after: Duration::from_millis(args.stuck_ms),
            resume_timeout: Duration::from_secs(args.resume_timeout_secs),
            preload_lead: Duration::from_secs(args.preload_secs),
        },
    );
    let app = api::router(Arc::clone(&node));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    tracing::info!(%addr, cache_dir = %ctx.work_dir().display(), "engine listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // upgraded connections are not the server's to wait for: the node closes them
            node.shutdown().await;
        })
        .await
        .context("server failed")?;
    tracing::info!("engine stopped");
    Ok(())
}

/// Imports a mounted cookie secret if it changed and keeps checking the session.
fn start_youtube_session(ctx: &Arc<MediaContext>) {
    let config = &ctx.config.youtube;
    if let Some(path) = &config.cookies_file {
        match ctx.youtube.store.import_file_if_changed(path) {
            Ok(Some(report)) => {
                tracing::info!(cookies = report.cookies, file = %path.display(), "imported the YouTube session")
            }
            Ok(None) => {
                tracing::debug!(file = %path.display(), "the YouTube cookie file is unchanged")
            }
            Err(e) => tracing::warn!(
                file = %path.display(),
                error = %entroximer_core::error::display_chain(&e),
                "cannot import the YouTube cookie file"
            ),
        }
    }
    if config.check_hours > 0 {
        let interval = Duration::from_secs(config.check_hours * 3600);
        tokio::spawn(youtube::maintain_session(Arc::clone(ctx), interval));
    }
}

/// Resolves on SIGINT, or SIGTERM as sent by `podman stop` / `docker stop`.
async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = interrupt => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!("cannot listen for SIGTERM: {e}");
                let _ = interrupt.await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
    tracing::info!("shutting down");
}

/// Exit status for container health checks: success when the local engine answers `/health`.
/// Uses a raw HTTP/1.1 request so the image needs no curl.
pub async fn check_health(args: &ListenArgs) -> anyhow::Result<()> {
    let host = if args.host.is_unspecified() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        args.host
    };
    let mut stream = TcpStream::connect((host, args.port))
        .await
        .with_context(|| format!("engine is not listening on {host}:{}", args.port))?;
    let request =
        format!("GET {HEALTH_PATH} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let status_line = response
        .split(|&b| b == b'\n')
        .next()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    anyhow::ensure!(
        status_line.starts_with("HTTP/1.1 200"),
        "unhealthy: {}",
        status_line.trim()
    );
    Ok(())
}
