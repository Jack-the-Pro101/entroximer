# Entroximer engine

The Rust audio engine: loads tracks (yt-dlp, ffmpeg), caches and seeks them, runs effect graphs
and will stream to Discord voice through a Lavalink v4 compatible API.

## Layout

| Crate | Contents |
|---|---|
| `crates/entroximer-core` | Producers, the packet timeline, demuxing/decoding, DSP graphs, the player |
| `crates/entroximer-server` | The `entroximer` binary: configuration, track sources, `serve` and `render` |

## Commands

```sh
# the engine server (currently the health endpoint; the Lavalink API builds on it)
cargo run -p entroximer-server -- serve

# play a track through the whole pipeline into a WAV file
cargo run -p entroximer-server -- render "https://www.youtube.com/watch?v=dQw4w9WgXcQ" \
    --start 0:30 --duration 20 --seek 5=2:00 --chain "lowpass(4000) => reverb(10, 2, 0.5)"
```

`render` accepts URLs (anything yt-dlp supports), searches such as `ytsearch:artist - title`,
and local files. Effects can also be given as a graph with `--graph '{"nodes": [...], "edges": [...]}'`
(see `crates/entroximer-core/src/dsp/graph.rs`).

Requires `ffmpeg`, a JavaScript runtime (Deno) for YouTube, and yt-dlp, either on the `PATH` or
managed by the engine (below).

## yt-dlp

YouTube changes break yt-dlp regularly, so the engine can own its yt-dlp instead of depending on
whatever is installed (`ENTROXIMER_YTDLP_MANAGED=true`, the default in the container image). It
installs yt-dlp into a Python virtual environment under the data directory on first start and
checks for a newer release every day. Each version gets its own directory and is switched in
atomically once it works, so running downloads are never disturbed.

```sh
entroximer ytdlp status            # installed version and path
entroximer ytdlp update            # install or update now, also while the engine is running
podman exec <container> entroximer ytdlp update
```

Use `ENTROXIMER_YTDLP_NIGHTLY=true` to follow nightly builds, which get YouTube fixes first, pin
with `ENTROXIMER_YTDLP_PACKAGE='yt-dlp[default]==<version>'`, or try a test build such as
yt-dlp's SABR branch with `ENTROXIMER_YTDLP_PACKAGE='yt-dlp[default] @ git+https://...'`.

## How seeking works

Every source is read forward-only, whether it is yt-dlp writing to a pipe, a file yt-dlp is
still writing (YouTube SABR, which cannot go to stdout), or ffmpeg. Its packets are indexed by
time in a per-track *timeline*, and all seeking happens there:

1. A position that is already downloaded plays instantly; this covers almost every seek once a
   typical song has downloaded, which takes a few seconds.
2. A position just ahead of the download is waited for when the download will reach it within
   two seconds at its measured speed.
3. Anything farther is re-opened at that position when the source can seek by time (YouTube
   `https` formats, through ffmpeg range requests on the media URL). The ranges merge as they
   meet, and gaps are filled ahead of playback.
4. Otherwise the download is waited for.

## Memory and disk

Packets spill to anonymous files in the cache directory as they arrive; RAM only holds a window
around the playhead plus a small index. The window reaches `ENTROXIMER_FORWARD_BIAS` times
further ahead than behind, and the same bias decides what is evicted first when a cap is
reached. When the cache is full the download pauses rather than being thrown away.

## Configuration

Every option is a flag or an environment variable (`entroximer --help`).

| Variable | Default | Meaning |
|---|---|---|
| `ENTROXIMER_HOST` / `ENTROXIMER_PORT` | `127.0.0.1` / `2333` | Listen address (`0.0.0.0` in the image) |
| `ENTROXIMER_CACHE_DIR` | `$TMPDIR/entroximer` | Spilled packets and downloads; keep it on fast disk, not tmpfs |
| `ENTROXIMER_NO_SPILL` | off | Keep the cache in RAM only |
| `ENTROXIMER_MEMORY_BEHIND_SECS` | `15` | Seconds kept in RAM behind the playhead |
| `ENTROXIMER_FORWARD_BIAS` | `4` | How much more is kept ahead than behind |
| `ENTROXIMER_MEMORY_MAX_MB` / `ENTROXIMER_DISK_MAX_MB` | `8` / `1024` | Per-track caps |
| `ENTROXIMER_DATA_DIR` | `$XDG_DATA_HOME/entroximer` | Persistent state: the managed yt-dlp |
| `ENTROXIMER_YTDLP_MANAGED` | off (on in the image) | Let the engine install and update yt-dlp |
| `ENTROXIMER_YTDLP_PACKAGE` | `yt-dlp[default]` | pip requirement for the managed install |
| `ENTROXIMER_YTDLP_NIGHTLY` | off | Install nightly builds |
| `ENTROXIMER_YTDLP_UPDATE_HOURS` | `24` | Update check interval; `0` installs once and never updates |
| `ENTROXIMER_YTDLP`, `ENTROXIMER_FFMPEG` | `yt-dlp`, `ffmpeg` | Executables (`ENTROXIMER_YTDLP` only when not managed) |
| `ENTROXIMER_PYTHON` | `python3` | Python used for the managed install |
| `ENTROXIMER_YTDLP_ARGS` | | Extra yt-dlp arguments, e.g. `--extractor-args youtube:formats=duplicate` |
| `ENTROXIMER_LOG_FORMAT` | `pretty` | `json` for log collectors |
| `RUST_LOG` | `info` | Log verbosity, e.g. `entroximer_core=debug` |

Each running engine works in its own `run-*` directory inside the cache directory and removes it
on exit; directories left by a crashed engine are cleaned up by the next one to start.

## Containers

```sh
podman build -t entroximer-engine .            # or: docker build -f Containerfile -t entroximer-engine .
podman compose up --build                      # from the repository root, using compose.yaml
```

The image runs as an unprivileged user under `tini`, listens on port 2333, logs JSON, shuts down
cleanly on `SIGTERM`, and has a health check (`entroximer healthcheck`). It has two volumes:
`/var/lib/entroximer` holds the managed yt-dlp, which therefore survives restarts and image
upgrades, and `/var/cache/entroximer` holds the packet cache and belongs on fast disk. On SELinux
hosts add `:Z` to bind mounts. yt-dlp is not in the image, so YouTube fixes never need a rebuild.

## Development

```sh
cargo test --workspace                       # needs ffmpeg with libopus for the pipeline tests
cargo clippy --workspace --all-targets
RUST_LOG=entroximer_core=debug cargo test    # shows producer and seeking decisions
```
