//! `entroximer serve`: the long-running engine.
//!
//! For now this serves the health endpoint used by container orchestration, maintains the
//! managed yt-dlp, and shuts down gracefully on SIGTERM/SIGINT; the Lavalink v4 compatible API is
//! added on top of it.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::routing::get;
use clap::Args;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::MediaContext;
use crate::managed_ytdlp::YtDlpBinary;
use crate::youtube;

pub const HEALTH_PATH: &str = "/health";

#[derive(Clone, Debug, Args)]
pub struct ServeArgs {
    /// Address to listen on; `0.0.0.0` inside containers.
    #[arg(long, env = "ENTROXIMER_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    #[arg(long, env = "ENTROXIMER_PORT", default_value_t = 2333)]
    pub port: u16,
}

pub async fn run(args: ServeArgs, ctx: Arc<MediaContext>) -> anyhow::Result<()> {
    if let YtDlpBinary::Managed(managed) = &ctx.ytdlp {
        let hours = ctx.config.ytdlp.update_hours;
        let interval = (hours > 0).then(|| Duration::from_secs(hours * 3600));
        let managed = Arc::clone(managed);
        tokio::spawn(async move { managed.maintain(interval).await });
    }
    start_youtube_session(&ctx);

    let app = Router::new().route(HEALTH_PATH, get(|| async { "ok" }));
    let addr = SocketAddr::new(args.host, args.port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    tracing::info!(%addr, cache_dir = %ctx.work_dir().display(), "engine listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
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
pub async fn check_health(args: &ServeArgs) -> anyhow::Result<()> {
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
