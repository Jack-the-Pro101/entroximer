//! `entroximer render`: plays a track through the full pipeline into a WAV file, optionally
//! seeking and applying effects on the way. Useful for testing sources, seeking and graphs
//! without Discord.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use entroximer_core::dsp::{
    CHANNELS, Effects, GraphSpec, NodeRegistry, PresetRegistry, PresetSelection, parse_chain,
};
use entroximer_core::player::{PlaybackError, Player, SAMPLE_RATE, Status};
use entroximer_core::timeline::{Ticks, duration, ticks};
use hound::{SampleFormat, WavSpec, WavWriter};
use serde_json::{Map, Value};
use tokio::runtime::Handle;

use crate::config::MediaContext;
use crate::managed_ytdlp::{YtDlpBinary, log_outcome};

const BLOCK_FRAMES: usize = 960;

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
    #[arg(long, conflicts_with_all = ["graph", "preset"])]
    chain: Option<String>,

    /// Effect graph as JSON: `{"nodes": [...], "edges": [...]}`.
    #[arg(long, conflicts_with = "preset")]
    graph: Option<String>,

    /// A preset, optionally with parameters as JSON: `nightcore` or `bass_boost={"gain_db": 9}`.
    #[arg(long)]
    preset: Option<String>,
}

pub async fn run(args: RenderArgs, ctx: &MediaContext) -> anyhow::Result<()> {
    let effects = build_effects(&args)?;
    if let YtDlpBinary::Managed(managed) = &ctx.ytdlp
        && managed.installed_version().await.is_none()
    {
        tracing::info!("installing yt-dlp");
        log_outcome(&managed.update().await.context("cannot install yt-dlp")?);
    }
    let track = ctx
        .sources
        .resolve_query(&args.input, ctx)
        .await
        .with_context(|| format!("cannot load `{}`", args.input))?;
    let info = &track.track.info;
    tracing::info!(
        title = %info.title,
        author = info.author.as_deref().unwrap_or("unknown"),
        source = %info.source_name,
        caps = ?track.producer.caps(),
        "rendering"
    );

    let player_cfg = ctx.player_config();
    let rt = Handle::current();
    tokio::task::spawn_blocking(move || {
        let player = Player::new(
            rt,
            track.producer,
            player_cfg,
            args.start.map(|t| ticks(t.0)),
        );
        render_to_wav(player, effects, &args)
    })
    .await
    .context("renderer panicked")?
}

fn build_effects(args: &RenderArgs) -> anyhow::Result<Option<Effects>> {
    let registry = NodeRegistry::with_builtins();
    let spec: Option<GraphSpec> = match (&args.chain, &args.graph, &args.preset) {
        (Some(chain), ..) => Some(parse_chain(&registry, chain).context("invalid --chain")?),
        (_, Some(json), _) => Some(serde_json::from_str(json).context("invalid --graph JSON")?),
        (.., Some(preset)) => Some(
            PresetRegistry::with_builtins()
                .build(&parse_preset(preset)?)
                .context("invalid --preset")?,
        ),
        (None, None, None) => None,
    };
    if let Some(spec) = &spec {
        tracing::info!(graph = %serde_json::to_string(spec)?, "effects");
    }
    spec.map(|spec| {
        registry
            .compile(&spec, f64::from(SAMPLE_RATE))
            .context("invalid effect graph")
    })
    .transpose()
}

/// `name` or `name={"param": value}`.
fn parse_preset(text: &str) -> anyhow::Result<PresetSelection> {
    let (name, params) = match text.split_once('=') {
        Some((name, json)) => (
            name,
            Some(
                serde_json::from_str::<Map<String, Value>>(json)
                    .context("preset parameters must be a JSON object")?,
            ),
        ),
        None => (text, None),
    };
    Ok(PresetSelection {
        name: name.trim().to_owned(),
        params,
    })
}

/// The player as a source that never runs dry: it waits for media while buffering, as an
/// offline renderer can, and fills with silence once the track has ended.
struct PlayerSource {
    player: Player,
    ended: bool,
    error: Option<PlaybackError>,
}

impl PlayerSource {
    fn fill(&mut self, out: &mut [f32]) {
        let mut filled = 0;
        while filled < out.len() && !self.ended && self.error.is_none() {
            match self.player.render(&mut out[filled..]) {
                Ok(rendered) => {
                    filled += rendered.frames * CHANNELS;
                    match rendered.status {
                        Status::Playing => {}
                        Status::Buffering => self.player.wait_for_media(Duration::from_millis(50)),
                        Status::Ended => self.ended = true,
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        out[filled..].fill(0.0);
    }
}

fn render_to_wav(
    player: Player,
    mut effects: Option<Effects>,
    args: &RenderArgs,
) -> anyhow::Result<()> {
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
    let mut source = PlayerSource {
        player,
        ended: false,
        error: None,
    };

    while !source.ended {
        while let Some(seek) = seeks.next_if(|s| ticks(s.at) <= written) {
            tracing::info!(at = ?seek.at, to = ?seek.to, "seeking");
            source.player.seek(ticks(seek.to))?;
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

        let block = &mut block[..frames * CHANNELS];
        match &mut effects {
            Some(effects) => effects.render(block, |input| source.fill(input)),
            None => source.fill(block),
        }
        if let Some(error) = source.error.take() {
            return Err(error.into());
        }
        for &sample in block.iter() {
            wav.write_sample(sample)?;
        }
        written += frames as Ticks;
    }
    wav.finalize()?;
    let stats = source.player.cache_stats();
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    fn parses_presets_with_and_without_parameters() {
        let plain = parse_preset("nightcore").unwrap();
        assert_eq!(plain.name, "nightcore");
        assert_eq!(plain.params, None);
        let tuned = parse_preset(r#"bass_boost={"gain_db": 9}"#).unwrap();
        assert_eq!(tuned.params.unwrap()["gain_db"], 9);
        assert!(parse_preset("bass_boost=9").is_err());
    }

    #[test]
    fn parses_seeks() {
        let seek: SeekAt = "5=1:30".parse().unwrap();
        assert_eq!((seek.at.as_secs(), seek.to.as_secs()), (5, 90));
        assert!("5".parse::<SeekAt>().is_err());
    }
}
