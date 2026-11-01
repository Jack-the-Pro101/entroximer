//! `entroximer render`: plays a track through the full pipeline into a WAV file, optionally
//! seeking and applying effects on the way. Useful for testing sources, seeking and graphs
//! without Discord.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use entroximer_core::dsp::{GraphRunner, GraphSpec, NodeRegistry, parse_chain};
use entroximer_core::player::{Player, SAMPLE_RATE, Status};
use entroximer_core::timeline::{Ticks, duration, ticks};
use hound::{SampleFormat, WavSpec, WavWriter};
use tokio::runtime::Handle;

use crate::config::MediaContext;
use crate::managed_ytdlp::{YtDlpBinary, log_outcome};
use crate::sources;

const BLOCK_FRAMES: usize = 960;
const CHANNELS: usize = 2;

#[derive(Debug, Args)]
pub struct RenderArgs {
    /// A URL, a search such as `ytsearch:artist - title`, or a local file.
    input: String,

    #[arg(short, long, default_value = "output.wav")]
    output: PathBuf,

    /// Where playback starts, as seconds or `[hh:]mm:ss`.
    #[arg(long)]
    start: Option<Timestamp>,

    /// Seek while rendering: `AT=TO` jumps to TO once AT of audio has been written. Repeatable.
    #[arg(long = "seek")]
    seeks: Vec<SeekAt>,

    /// Stop after writing this much audio.
    #[arg(long)]
    duration: Option<Timestamp>,

    /// Effect chain, e.g. `lowpass(800) => reverb(10, 2, 0.5)`.
    #[arg(long, conflicts_with = "graph")]
    chain: Option<String>,

    /// Effect graph as JSON: `{"nodes": [...], "edges": [...]}`.
    #[arg(long)]
    graph: Option<String>,
}

pub async fn run(args: RenderArgs, ctx: &MediaContext) -> anyhow::Result<()> {
    let graph = build_graph(&args)?;
    if let YtDlpBinary::Managed(managed) = &ctx.ytdlp
        && managed.installed_version().await.is_none()
    {
        tracing::info!("installing yt-dlp");
        log_outcome(&managed.update().await.context("cannot install yt-dlp")?);
    }
    let track = sources::resolve(&args.input, ctx)
        .await
        .with_context(|| format!("cannot load `{}`", args.input))?;
    tracing::info!(title = %track.info.title, author = %track.info.author, caps = ?track.producer.caps(), "rendering");

    let player_cfg = ctx.player_config();
    let rt = Handle::current();
    tokio::task::spawn_blocking(move || {
        let mut player = Player::new(
            rt,
            track.producer,
            player_cfg,
            args.start.map(|t| ticks(t.0)),
        );
        player.set_graph(graph);
        render_to_wav(&mut player, &args)
    })
    .await
    .context("renderer panicked")?
}

fn build_graph(args: &RenderArgs) -> anyhow::Result<Option<GraphRunner>> {
    let registry = NodeRegistry::with_builtins();
    let spec: Option<GraphSpec> = match (&args.chain, &args.graph) {
        (Some(chain), _) => Some(parse_chain(&registry, chain).context("invalid --chain")?),
        (None, Some(json)) => Some(serde_json::from_str(json).context("invalid --graph JSON")?),
        (None, None) => None,
    };
    spec.map(|spec| {
        let net = registry
            .compile(&spec, f64::from(SAMPLE_RATE))
            .context("invalid effect graph")?;
        Ok(GraphRunner::new(Box::new(net)))
    })
    .transpose()
}

fn render_to_wav(player: &mut Player, args: &RenderArgs) -> anyhow::Result<()> {
    let spec = WavSpec {
        channels: CHANNELS as u16,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut wav = WavWriter::create(&args.output, spec)
        .with_context(|| format!("cannot create {}", args.output.display()))?;

    let mut seeks: Vec<SeekAt> = args.seeks.clone();
    seeks.sort_by_key(|s| s.at);
    let mut seeks = seeks.into_iter().peekable();
    let limit = args.duration.map(|d| ticks(d.0));
    let mut written: Ticks = 0;
    let mut block = vec![0.0; BLOCK_FRAMES * CHANNELS];

    loop {
        while let Some(seek) = seeks.next_if(|s| ticks(s.at) <= written) {
            tracing::info!(at = ?seek.at, to = ?seek.to, "seeking");
            player.seek(ticks(seek.to))?;
        }
        // stop exactly at the next seek or the duration limit
        let next_stop = [seeks.peek().map(|s| ticks(s.at)), limit]
            .into_iter()
            .flatten()
            .min();
        let frames = match next_stop {
            Some(stop) => usize::try_from(stop - written)?.min(BLOCK_FRAMES),
            None => BLOCK_FRAMES,
        };
        if frames == 0 {
            break;
        }

        let rendered = player.render(&mut block[..frames * CHANNELS])?;
        for &sample in &block[..rendered.frames * CHANNELS] {
            wav.write_sample(sample)?;
        }
        written += rendered.frames as Ticks;
        match rendered.status {
            Status::Playing => {}
            Status::Buffering => player.wait_for_media(Duration::from_millis(50)),
            Status::Ended => break,
        }
    }
    wav.finalize()?;
    let stats = player.cache_stats();
    tracing::info!(
        output = %args.output.display(),
        seconds = duration(written).as_secs_f64(),
        cache_ram_bytes = stats.memory_bytes,
        cache_disk_bytes = stats.disk_bytes,
        "done"
    );
    Ok(())
}

/// A point in the media, written as seconds (`90`, `90.5`) or `[hh:]mm:ss[.fff]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timestamp(pub Duration);

#[derive(Debug, thiserror::Error)]
#[error("invalid time `{0}`, expected seconds or [hh:]mm:ss")]
pub struct InvalidTimestamp(String);

impl FromStr for Timestamp {
    type Err = InvalidTimestamp;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidTimestamp(s.to_owned());
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() > 3 {
            return Err(invalid());
        }
        let seconds = parts.iter().try_fold(0.0, |acc, part| {
            let value: f64 = part.parse().map_err(|_| invalid())?;
            Ok(acc * 60.0 + value)
        })?;
        Duration::try_from_secs_f64(seconds)
            .map(Self)
            .map_err(|_| invalid())
    }
}

/// `AT=TO`: once `AT` of audio has been written, seek to `TO`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeekAt {
    at: Duration,
    to: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidSeek {
    #[error("expected AT=TO, e.g. 5=1:30")]
    Format,
    #[error(transparent)]
    Time(#[from] InvalidTimestamp),
}

impl FromStr for SeekAt {
    type Err = InvalidSeek;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (at, to) = s.split_once('=').ok_or(InvalidSeek::Format)?;
        Ok(Self {
            at: at.parse::<Timestamp>()?.0,
            to: to.parse::<Timestamp>()?.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        let secs = |s: &str| s.parse::<Timestamp>().map(|t| t.0.as_secs_f64()).ok();
        assert_eq!(secs("90"), Some(90.0));
        assert_eq!(secs("1:30.5"), Some(90.5));
        assert_eq!(secs("1:02:03"), Some(3723.0));
        assert_eq!(secs("1:2:3:4"), None);
        assert_eq!(secs("-5"), None);
        assert_eq!(secs("x"), None);
    }

    #[test]
    fn parses_seeks() {
        let seek: SeekAt = "5=1:30".parse().unwrap();
        assert_eq!((seek.at.as_secs(), seek.to.as_secs()), (5, 90));
        assert!("5".parse::<SeekAt>().is_err());
    }
}
