//! Where clients' links may lead the engine: a page can name any media URL, so what yt-dlp finds
//! is fetched through the proxy for clients' links, which refuses the engine's own machine and
//! private networks. A fake yt-dlp stands in for a page that names media on the engine's host.

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use axum::Router;
use axum::http::header;
use axum::routing::get;
use clap::Parser;
use entroximer_server::config::{MediaConfig, MediaContext};
use futures_util::StreamExt;

const PAGE: &str = "https://soundcloud.com/someone/a-track";

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    media: MediaConfig,
}

struct Engine {
    dir: tempfile::TempDir,
    ctx: MediaContext,
}

impl Engine {
    /// An engine whose yt-dlp finds, on any page, the media at `media_url`.
    fn new(media_url: &str, flags: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("yt-dlp");
        let info = format!(
            r#"{{"id":"1","title":"A track","extractor_key":"Soundcloud","duration":1,"thumbnail":"https://example.invalid/cover.jpg","formats":[{{"format_id":"hls_mp3","ext":"mp3","acodec":"mp3","vcodec":"none","abr":128,"protocol":"m3u8_native","url":"{media_url}"}}]}}"#
        );
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{calls}'\nprintf '%s' '{info}'\n",
                calls = dir.path().join("calls").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let mut args = vec![
            "entroximer".to_owned(),
            "--ytdlp".into(),
            script.to_string_lossy().into_owned(),
            "--data-dir".into(),
            path("data"),
            "--cache-dir".into(),
            path("cache"),
        ];
        args.extend(flags.iter().map(|flag| (*flag).to_owned()));
        let ctx = MediaContext::new(Cli::parse_from(args).media).unwrap();
        Self { dir, ctx }
    }

    /// The arguments of each yt-dlp run so far.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Resolves `page` and downloads what it plays: the media's size, or why it did not come.
    async fn play(&self, page: &str) -> Result<usize, String> {
        let track = self
            .ctx
            .sources
            .resolve_query(page, None, &self.ctx)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = track.producer.open(None).await.map_err(|e| e.to_string())?;
        let mut size = 0;
        while let Some(chunk) = stream.bytes.next().await {
            size += chunk.map_err(|e| e.to_string())?.len();
        }
        Ok(size)
    }
}

/// Serves a short tone on a free loopback port; returns its URL.
async fn serve_tone() -> String {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tone.mp3");
    let made = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
        .args(["-c:a", "libmp3lame"])
        .arg(&file)
        .status()
        .expect("ffmpeg must be installed to run these tests");
    assert!(made.success());
    let tone = fs::read(&file).unwrap();
    let router = Router::new().route(
        "/tone.mp3",
        get(move || {
            let tone = tone.clone();
            async move { ([(header::CONTENT_TYPE, "audio/mpeg")], tone) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}/tone.mp3")
}

#[tokio::test(flavor = "multi_thread")]
async fn media_a_page_names_on_the_engines_host_is_not_fetched() {
    let engine = Engine::new(&serve_tone().await, &[]);
    let refused = engine
        .play(PAGE)
        .await
        .expect_err("the media is on the engine's own machine");
    assert!(refused.contains("403"), "{refused}");

    let proxy = engine.ctx.link_proxy().expect("links have their proxy");
    let calls = engine.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].contains(&format!("--proxy {proxy}")),
        "yt-dlp reads the page through the proxy too: {calls:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn private_urls_play_where_the_operator_allows_them() {
    let engine = Engine::new(&serve_tone().await, &["--allow-private-urls"]);
    let size = engine.play(PAGE).await.unwrap();
    assert!(size > 1000, "{size} bytes");
    assert!(engine.ctx.link_proxy().is_none());
    let calls = engine.calls();
    assert!(!calls[0].contains("--proxy"), "{calls:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn files_on_the_engines_host_play_through_ffmpeg_without_the_proxy() {
    // AC-3 is not among what the engine's demuxer reads, so ffprobe and ffmpeg read the file
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tone.ac3");
    let made = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
        .args(["-c:a", "ac3"])
        .arg(&file)
        .status()
        .unwrap();
    assert!(made.success());

    let engine = Engine::new("https://example.invalid/", &["--allow-local-files"]);
    assert!(engine.ctx.link_proxy().is_some(), "links are still guarded");
    let size = engine.play(&file.to_string_lossy()).await.unwrap();
    assert!(size > 1000, "{size} bytes");
    assert!(engine.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn youtube_is_reached_as_before() {
    let engine = Engine::new("https://example.invalid/playlist.m3u8", &[]);
    engine
        .ctx
        .sources
        .resolve_query(
            "https://www.youtube.com/watch?v=aaaaaaaaaaa",
            None,
            &engine.ctx,
        )
        .await
        .unwrap();
    let calls = engine.calls();
    assert!(!calls.is_empty());
    // its PO token provider is on a private address, usually the engine's own machine
    assert!(
        calls.iter().all(|call| !call.contains("--proxy")),
        "{calls:?}"
    );
}
