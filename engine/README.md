# Entroximer engine

The Rust audio engine: loads tracks (yt-dlp, ffmpeg, Spotify metadata played from YouTube), caches
and seeks them, runs effect graphs and presets, and plays them into Discord voice channels,
controlled by bots over a WebSocket protocol inspired by (but not compatible with) Lavalink v4.

## Layout

| Crate | Contents |
|---|---|
| `crates/entroximer-core` | Producers, the packet timeline, demuxing/decoding, effects (graphs, presets, playback rate), the player, the gapless deck |
| `crates/entroximer-server` | The `entroximer` binary: configuration, track sources, the client API, voice, `serve` and `render` |

## Commands

```sh
# the engine server; with a password, clients must send it
ENTROXIMER_PASSWORD=change-me cargo run -p entroximer-server -- serve

# play a track through the whole pipeline into a WAV file
cargo run -p entroximer-server -- render "https://www.youtube.com/watch?v=dQw4w9WgXcQ" \
    --start 0:30 --duration 20 --seek 5=2:00 --chain "lowpass(4000) => reverb(10, 2, 0.5, 0.3)"
```

`render` accepts URLs (anything yt-dlp supports), searches such as `ytsearch:artist - title`,
and local files. Effects can also be a graph, `--graph '{"nodes": [...], "edges": [...]}'`, or a
preset, `--preset nightcore` or `--preset 'bass_boost={"gain_db": 9}'` (see [Effects](#effects)).

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

## Client protocol

Bots talk to the engine over one WebSocket, `/v1/ws`, sending the headers `User-Id` (the bot's
Discord ID), `Authorization` (the engine's password, if it has one) and optionally `Client-Name`
and `Session-Id` (to resume). The same connection carries requests, their responses and events, in
order: the events a request causes always arrive before its response.

```json
→ {"id": 1, "op": "queue.add", "params": {"guildId": "123", "tracks": [{"query": "ytsearch:..."}]}}
← {"type": "event", "event": "queueChanged", "guildId": "123", "version": 1, "change": {"type": "added", ...}}
← {"type": "response", "id": 1, "result": {...}}
← {"type": "event", "event": "trackStart", "guildId": "123", "track": {...}, "itemId": "..."}
```

| `op` | Does |
|---|---|
| `tracks.load` | Loads a link, a search (`ytsearch:terms`, or plain text with the default search) or, if allowed, a local path: a `track`, a `playlist`, `search` results or `empty` |
| `tracks.search` | Searches one source by its prefix (`ytsearch`, `ytmsearch`, `scsearch`, `spsearch`), up to 50 results |
| `tracks.decode` | Reads tracks back from their `encoded` form |
| `presets.build` | The effect graph a preset builds (`name`, optional `params`), without applying it |
| `player.update` | Creates or changes a guild's player: `track` (plays now; `null` stops), `position`, `endTime`, `paused`, `volume`, `graph` or `preset` (effects), `voice`. A refused update changes nothing |
| `player.get` / `player.destroy` | Reads a player, or stops it and leaves the voice channel |
| `queue.get` | A page of a player's queue |
| `queue.add` | Adds tracks (encoded, or queries to load) at an index; with nothing playing, the first item starts |
| `queue.remove` / `queue.move` / `queue.clear` / `queue.shuffle` | Edit the queue |
| `queue.skip` | Plays the next item (or `toItem`), instantly if it is prepared |
| `queue.configure` | Sets `repeat`: `off`, `track` or `queue` |
| `session.configure` | Sets how long players wait, paused, for a disconnected client to resume |
| `node.info` / `node.stats` | Versions, sources and their searches, effect node types and presets, the YouTube session state; load and cache usage |

Events: `trackStart`, `trackEnd` (with a reason), `trackError` and `trackStuck`, each with the
track's queue `itemId`; `queueChanged` (one change, numbered by `version`) and `queueEnd`;
`playerUpdate` (position, buffered ranges, buffering, connection; every few seconds and on
changes), `voiceClosed`, `stats` and `youtubeSession`. Errors carry stable codes such as
`age_restricted`, `sign_in_required`, `unavailable` or `no_results`.

When a client's connection drops, its players pause where they are. The engine pings every
client every 20 s, so a connection that silently died (say, the bot's network went away) counts as
dropped after a minute without an answer; WebSocket libraries answer pings by themselves. If it
reconnects with its
`Session-Id` within the resume timeout, playback continues from the same position, the client
receives the events it could not get meanwhile (periodic ones excepted) and each player's current
state; otherwise the players are destroyed. Playback also holds its position while the voice
connection to Discord reconnects.

The engine bounds what one client can make it hold: messages up to 4 MiB, queues up to 10 000
items, graphs up to 16 nodes and 64 edges, and resume timeouts up to a day. A request that
asks for more is refused with `bad_request`.

When the engine shuts down (`SIGTERM`, `SIGINT`), `/health` answers 503, new connections are
refused, every player ends (clients get `trackEnd` with reason `cleanup`) and leaves its voice
channel, and each connection then closes with code 1001 ("going away"). Clients reconnect, find
their session gone after the restart, and start over; the TypeScript client reports this as
`playersLost`.

## Sources

Tracks carry the same metadata wherever they come from: title, author (and its link), album,
length, artwork, ISRC, whether they are streams or seekable, and their source. Search results
and playlist entries come from quick listings and are resolved only when they play; resolving
can fill in more (e.g. a YouTube Music result's artist and length), which reaches clients as a
queue update.

| Source | Links | Search |
|---|---|---|
| `youtube` | youtube.com, youtu.be, music.youtube.com; videos and playlists | `ytsearch`, `ytmsearch` (YouTube Music songs) |
| `soundcloud` | soundcloud.com tracks and sets | `scsearch` |
| `newgrounds`, `bandcamp` | their track and collection pages | |
| `http` | Direct links to audio files and internet radio (MP3, Ogg, FLAC, WAV, M4A, HLS, ...), described from their tags | |
| `generic` | Any other page yt-dlp can extract | |
| `spotify` | open.spotify.com tracks, albums, playlists and artists, `spotify:` URIs; only with a Spotify app (below) | `spsearch` |
| `local` | Files on the engine's host; only with `ENTROXIMER_ALLOW_LOCAL_FILES` | |

Everything but `http`, `local` and Spotify's metadata goes through yt-dlp. `http` and `generic` refuse links to the
engine's own machine and private networks (unless `ENTROXIMER_ALLOW_PRIVATE_URLS`), so clients
cannot use the engine to reach internal services. Newgrounds currently refuses automated
requests; it works as far as Newgrounds lets it.

## Spotify

Spotify's audio is DRM-protected, so like Lavalink's LavaSrc the engine only reads Spotify's
catalog and plays every Spotify track from its recording on YouTube. Tracks keep Spotify's title,
artists, album, artwork, ISRC and link; only their length becomes that of what plays.

1. Create an app at <https://developer.spotify.com/dashboard> (its owner needs Spotify Premium)
   and give the engine its credentials: `ENTROXIMER_SPOTIFY_CLIENT_ID` and
   `ENTROXIMER_SPOTIFY_CLIENT_SECRET`. Without them there is no `spotify` provider. At startup
   the engine logs its providers and whether Spotify accepts the credentials.
2. Optionally set `ENTROXIMER_SPOTIFY_MARKET` (the country whose catalog is read, `US` by default).

A track is matched when it is first played or preloaded: the searches of `ENTROXIMER_SPOTIFY_MIRROR`
run in order (by default `ytsearch:"{isrc}"`, `ytmsearch:{artist} {title}`, `ytsearch:{artist} - {title}`),
and the best result whose title shares most of the track's words, whose length is within 7 s or
10 % of it and that is the same kind of recording (a "live" or "remix" on one side only rules it
out) plays. Searching by ISRC finds the exact recording when YouTube knows it; its results are
checked like the others, because it returns unrelated videos as well. Matches are remembered for
a day.

Spotify restricts apps in its default development mode (since February 2026): searches return at
most ten results per request (the engine pages through them), playlists only list their tracks to
apps with extended quota (the engine then explains this instead of loading them), and artists'
top tracks are unavailable, so an artist link loads what a search for the artist finds.

## Effects

A player's effects are a graph of nodes between its tracks and the voice connection; they carry
across tracks, so a reverb's tail rings into the next one. `node.info` lists the node types with
their parameters and ranges:

| Nodes | |
|---|---|
| `volume`, `limiter`, `drive`, `channel_mix` | Gain, peak limiting, saturation, mixing the channels (mono, swapped, center removed) |
| `lowpass`, `highpass`, `bandpass`, `bell`, `lowshelf`, `highshelf` | Filters and equalizer bands |
| `reverb`, `echo`, `chorus`, `flanger`, `phaser` | Space and modulation; `reverb` and `echo` have a dry/wet `mix` |
| `tremolo`, `vibrato`, `auto_pan`, `pitch` | Periodic volume and pitch, side-to-side movement ("8D"), pitch shift at the same speed |

```json
{ "rate": 1.25,
  "nodes": [{ "id": "low", "type": "lowpass", "params": { "cutoff": 800 } }, { "id": "room", "type": "reverb" }] }
```

Without `edges` the nodes form a chain; with them, any graph (see `GraphSpec` in
`crates/entroximer-core/src/dsp/graph.rs`). `rate` (0.5 to 2) plays the tracks faster and higher
or slower and lower, like a turntable: it resamples what feeds the graph, so the playhead moves
`rate` times as fast, and it carries across tracks without gaps.

Presets build ready-made graphs: `bass_boost`, `nightcore`, `sped_up`, `vaporwave`,
`slowed_reverb`, `8d`, `karaoke`, `lofi`, `phonograph`, `telephone`, `chipmunk`, `deep` and
`concert_hall`, several with parameters (`bass_boost` takes `gain_db` and `frequency`). Their
nodes have readable ids, e.g. `bass_boost` is a `subsonic` high-pass, a `headroom` gain, the `bass`
shelf, a `mud` cut and a `limiter`. `player.update` with `preset` applies one, and the player's
`graph` then shows its nodes, ready to be changed or extended and set again as `graph`;
`presets.build` returns the graph without applying it.

## Queues

Each player plays its queue by itself. Once the current track has less than
`ENTROXIMER_PRELOAD_SECS` left, the engine resolves the next item and starts downloading it, so
it follows without a gap, whatever the client does meanwhile; if the queue changes so that
another item comes next, that one is prepared instead. Items leave the queue when they start.
Repeating a `track` plays it again; repeating the `queue` sends finished tracks to its end.

An item that fails to load is reported with `trackError` and removed, and the queue goes on;
after five failures in a row it waits until the client changes something. Clients mirror the
queue from `queueChanged` events and fetch it again (`queue.get`) when a version is skipped.
Queues live in memory: they survive client reconnects but not engine restarts.

Plain HTTP serves `/health`, `GET /v1/info` and `GET|PUT|DELETE /v1/youtube/session` (the
cookies.txt as the `PUT` body). The protocol's types are in
`crates/entroximer-server/src/api/protocol.rs`; `cargo test` exports them as TypeScript to
`client/src/generated`.

## Signing in to YouTube

Age-restricted, members-only and bot-checked videos need a signed-in account. YouTube no longer
accepts OAuth from yt-dlp, so the engine uses the cookies of a browser session:

1. In a private browser window, sign into a dedicated adult YouTube account (YouTube may restrict
   accounts used this way), open `https://www.youtube.com/robots.txt`, and export the cookies with
   a cookies.txt extension. Close the window without signing out, so YouTube does not rotate them.
2. Give them to the engine, from anywhere that can reach it:

   ```sh
   entroximer youtube cookies import cookies.txt
   ssh host 'podman exec -i entroximer entroximer youtube cookies import -' < cookies.txt
   ```

The import checks the session with YouTube right away. By default (`ENTROXIMER_YOUTUBE_AUTH=fallback`)
every video is tried anonymously first and the session is only sent when YouTube demands it,
which keeps the account out of most traffic. Cookies YouTube rotates are merged back after every
yt-dlp run, and the session is re-checked every 12 hours; when YouTube stops accepting it the
engine logs a warning and tracks needing it fail with instructions until fresh cookies are
imported. `entroximer youtube cookies status|check|remove` manage it; a cookie file mounted as a
secret (`ENTROXIMER_YOUTUBE_COOKIES_FILE`) is imported whenever it changes.

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

Packets are stored in blocks of about a second, which spill to anonymous files in the cache
directory as they complete. RAM only holds a window around the playhead plus an index of the
blocks: about 3 MB for ten hours of audio, which is downloaded to disk as fast as the source
allows (about 640 MB for ten hours of YouTube Opus). The window reaches
`ENTROXIMER_FORWARD_BIAS` times further ahead than behind, and the same bias decides what is
evicted first when a cap is reached. When the cache is full the download pauses rather than
being thrown away.

yt-dlp runs as a separate process for every search and every track it loads; with YouTube's
JavaScript challenges, one peaks at up to 350 MB. `ENTROXIMER_YTDLP_MAX_RUNS` bounds how many run
at once, separately for searches, so that searching as users type never holds up playback.

glibc keeps memory the engine freed in one arena per thread, so without a limit the engine's
memory grows with every track played. The container image, the Nix package and the dev shell
set `MALLOC_ARENA_MAX=2`; set it too wherever you run the binary yourself.

## Configuration

Every option is a flag or an environment variable (`entroximer --help`).

| Variable | Default | Meaning |
|---|---|---|
| `ENTROXIMER_PASSWORD` | none | What clients must send as `Authorization`; without it any client that can reach the engine may use it |
| `ENTROXIMER_HOST` / `ENTROXIMER_PORT` | `127.0.0.1` / `2333` | Listen address (`0.0.0.0` in the image) |
| `ENTROXIMER_PLAYER_UPDATE_SECS` | `5` | Interval of `playerUpdate` events |
| `ENTROXIMER_STUCK_MS` | `10000` | Wait for media after which a track is reported stuck |
| `ENTROXIMER_RESUME_TIMEOUT_SECS` | `60` | How long a disconnected client's players wait, paused, before they are destroyed |
| `ENTROXIMER_PRELOAD_SECS` | `30` | How long before a track ends the next item of the queue starts loading |
| `ENTROXIMER_DEFAULT_SEARCH` | `ytsearch` | Search for queries that are neither links nor prefixed |
| `ENTROXIMER_PLAYLIST_LIMIT` | `500` | Most tracks loaded from one playlist |
| `ENTROXIMER_ALLOW_LOCAL_FILES` | off | Let clients play files on the engine's host |
| `ENTROXIMER_ALLOW_PRIVATE_URLS` | off | Let clients play links to private networks, e.g. a media server on the LAN |
| `ENTROXIMER_CACHE_DIR` | `$TMPDIR/entroximer` | Spilled packets and downloads; keep it on fast disk, not tmpfs |
| `ENTROXIMER_NO_SPILL` | off | Keep the cache in RAM only |
| `ENTROXIMER_MEMORY_BEHIND_SECS` | `15` | Seconds kept in RAM behind the playhead |
| `ENTROXIMER_FORWARD_BIAS` | `4` | How much more is kept ahead than behind |
| `ENTROXIMER_MEMORY_MAX_MB` / `ENTROXIMER_DISK_MAX_MB` | `8` / `1024` | Per-track caps |
| `ENTROXIMER_DATA_DIR` | `$XDG_DATA_HOME/entroximer` | Persistent state: the managed yt-dlp and the YouTube session |
| `ENTROXIMER_YTDLP_MANAGED` | off (on in the image) | Let the engine install and update yt-dlp |
| `ENTROXIMER_YTDLP_PACKAGE` | `yt-dlp[default]` | pip requirement for the managed install |
| `ENTROXIMER_YTDLP_NIGHTLY` | off | Install nightly builds |
| `ENTROXIMER_YTDLP_UPDATE_HOURS` | `24` | Update check interval; `0` installs once and never updates |
| `ENTROXIMER_YTDLP`, `ENTROXIMER_FFMPEG`, `ENTROXIMER_FFPROBE` | `yt-dlp`, `ffmpeg`, `ffprobe` | Executables (`ENTROXIMER_YTDLP` only when not managed) |
| `ENTROXIMER_PYTHON` | `python3` | Python used for the managed install |
| `ENTROXIMER_SPOTIFY_CLIENT_ID` / `ENTROXIMER_SPOTIFY_CLIENT_SECRET` | none | A Spotify app's credentials, which enable Spotify links and `spsearch` |
| `ENTROXIMER_SPOTIFY_MARKET` | `US` | Country whose Spotify catalog is read |
| `ENTROXIMER_SPOTIFY_MIRROR` | see [Spotify](#spotify) | Comma-separated searches that find Spotify tracks on YouTube; `{isrc}`, `{title}`, `{artist}`, `{artists}` |
| `ENTROXIMER_YOUTUBE_AUTH` | `fallback` | When to send the YouTube session: `fallback`, `always`, `never` |
| `ENTROXIMER_YOUTUBE_COOKIES_FILE` | | cookies.txt imported whenever it changes, e.g. a secret |
| `ENTROXIMER_YOUTUBE_CHECK_HOURS` | `12` | Session check interval; `0` disables it |
| `ENTROXIMER_YOUTUBE_CHECK_VIDEO` | an age-restricted video | Video used to check the session |
| `ENTROXIMER_YTDLP_ARGS` | | Extra yt-dlp arguments, e.g. `--extractor-args youtube:formats=duplicate` |
| `ENTROXIMER_YTDLP_MAX_RUNS` | `2` | yt-dlp runs at once, counted separately for searches; downloads do not count |
| `ENTROXIMER_LOG_FORMAT` | `pretty` | `json` for log collectors |
| `RUST_LOG` | `info,songbird=warn` | Log verbosity, e.g. `info,entroximer_core=debug` |

Each running engine works in its own `run-*` directory inside the cache directory and removes it
on exit; directories left by a crashed engine are cleaned up by the next one to start.

## Containers

```sh
podman build -t entroximer-engine .            # or: docker build -f Containerfile -t entroximer-engine .
podman compose up --build                      # from the repository root; set ENTROXIMER_PASSWORD to require one
```

The image runs as an unprivileged user under `tini`, listens on port 2333, logs JSON, shuts down
cleanly on `SIGTERM`, and has a health check (`entroximer healthcheck`). It has two volumes:
`/var/lib/entroximer` holds the managed yt-dlp and the YouTube session, which therefore survive
restarts and image upgrades, and `/var/cache/entroximer` holds the packet cache and belongs on fast disk. On SELinux
hosts add `:Z` to bind mounts. yt-dlp is not in the image, so YouTube fixes never need a rebuild.

## Development

`nix develop` (or `nix-shell`) in the repository root provides the toolchain and every tool the
engine runs; elsewhere, install Rust, pkg-config, libopus, ffmpeg (with libopus), yt-dlp and Deno.

```sh
cargo test --workspace                       # needs ffmpeg with libopus for the media tests
cargo clippy --workspace --all-targets
RUST_LOG=entroximer_core=debug cargo test    # shows producer and seeking decisions
nix build                                    # the engine as a Nix package, tests included
```
