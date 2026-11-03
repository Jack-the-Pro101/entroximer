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
| `player.update` | Creates or changes a guild's player: `track` (plays now; `null` stops), `position`, `endTime`, `paused`, `volume`, `graph` or `preset` (effects), `voice`. These are the music's; a refused update changes nothing |
| `player.get` / `player.destroy` | Reads a player, or stops it and leaves the voice channel |
| `queue.get` | A page of a player's queue |
| `queue.add` | Adds tracks (encoded, or queries to load) at an index; with nothing playing, the first item starts |
| `queue.remove` / `queue.move` / `queue.clear` / `queue.shuffle` | Edit the queue |
| `queue.skip` | Plays the next item (or `toItem`), instantly if it is prepared |
| `queue.configure` | Sets `repeat` (`off`, `track` or `queue`) and `crossfade` (ms, up to 12 000) |
| `layer.update` / `layer.remove` | Creates or changes a layer besides the music (`volume`, `graph` or `preset`, `paused`, `mode`, `maxClips`, `duck`), or removes it; see [Layers and clips](#layers-and-clips) |
| `clip.play` / `clip.stop` | Plays a sound on a layer, outside the queue (`volume`, `position`, `endTime`, `fadeIn`, `fadeOut`, `loop`), or stops one or all of a layer's |
| `session.configure` | Sets how long players wait, paused, for a disconnected client to resume |
| `node.info` / `node.stats` | Versions, sources and their searches, effect node types and presets, the YouTube session state; load and cache usage |

Events: `trackStart`, `trackEnd` (with a reason), `trackError` and `trackStuck`, each with the
track's queue `itemId`; `queueChanged` (one change, numbered by `version`) and `queueEnd`;
`playerUpdate` (position, buffered ranges, buffering, connection; every few seconds and on
changes), `voiceClosed`, `stats` and `youtubeSession`; `clipStart`, `clipEnd`, `clipError` and
`clipStuck`, each with the clip's `layer` and `clipId`. Errors carry stable codes such as
`age_restricted`, `sign_in_required`, `unavailable` or `no_results`.

When a client's connection drops, its players pause where they are. The engine pings every
client every 20 s, so a connection that silently died (say, the bot's network went away) counts as
dropped after a minute without an answer; WebSocket libraries answer pings by themselves. If it
reconnects with its
`Session-Id` within the resume timeout, playback continues from the same position, the client
receives the events it could not get meanwhile (periodic ones excepted) and each player's current
state; otherwise the players are destroyed. Playback also holds its position while the voice
connection to Discord reconnects.

The engine bounds what one client can make it hold: messages up to 4 MiB, 256 requests running
at once per connection (it reads further ones as those finish), queues up to 10 000 items, graphs up to 16 nodes and 64 edges, 4 layers besides the music per player with 8 clips
playing at once and 32 waiting on each, fades up to 10 s, and resume timeouts up to a day. A
request that asks for more is refused with `bad_request`.

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
| `local` | Files on the engine's host, by their path; only with `ENTROXIMER_LOCAL_FILES_DIR` or `ENTROXIMER_ALLOW_LOCAL_FILES` (see [Local files](#local-files)) | |
| `upload` | Files clients uploaded, as `upload:<id>`; only with `ENTROXIMER_UPLOADS` (see [Uploads](#uploads)) | |

Everything but `http`, `local` and Spotify's metadata goes through yt-dlp. `http` and `generic` refuse links to the
engine's own machine and private networks (unless `ENTROXIMER_ALLOW_PRIVATE_URLS`), so clients
cannot use the engine to reach internal services. Newgrounds currently refuses automated
requests; it works as far as Newgrounds lets it.

### Local files

A bot's own sounds, such as a soundboard's, are best kept as files the engine plays by their
path. Clients may play the files within `ENTROXIMER_LOCAL_FILES_DIR` and no others: relative
paths are within it, and paths that lead out of it, through `..` or links, are refused.
`ENTROXIMER_ALLOW_LOCAL_FILES` instead allows any file the engine can read. Loading an absolute
path that is not a file clients may play fails with `not_found`.

Files of up to 64 MiB that the engine's own demuxer reads (Ogg, Opus, MP3, FLAC, WAV, WebM,
...) play straight from the file. What one holds (length, tags, whether it has a cover) is read
once and kept while the file is unchanged, so playing it again starts no process. Other files,
such as MP4s with their index at the end, play through ffprobe and ffmpeg each time.

### Artwork

Every track's `artworkUrl` comes from the first of these that has a picture, whatever the source:

1. The source: YouTube's thumbnail (always `https://i.ytimg.com/vi/<id>/hqdefault.jpg`, the same
   in searches, playlists and playback, so queued tracks do not change when they load),
   SoundCloud and Spotify artwork, or a thumbnail yt-dlp found.
2. For a playlist's tracks, the playlist's picture, e.g. a Bandcamp album's cover.
3. The page's own preview picture (`og:image`), as sites show it in link previews; fetched once
   per page (5 s, the page's head only), never from private addresses unless they are allowed.
4. For media files (direct links, local files), the cover embedded in the file. The engine
   extracts it as a JPEG of at most 600 px and serves it at `/v1/artwork/<key>`, which needs the
   `Authorization` header like the rest of the API. Such an `artworkUrl` is that path on the
   engine; clients fetch the image and, for Discord, upload it with the message.

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
| `volume`, `limiter`, `compressor`, `drive`, `channel_mix` | Gain, peak limiting, evening out the level, saturation, mixing the channels (mono, swapped, center removed) |
| `lowpass`, `highpass`, `bandpass`, `bell`, `lowshelf`, `highshelf` | Filters and equalizer bands |
| `reverb`, `echo`, `chorus`, `flanger`, `phaser` | Space and modulation; `reverb` and `echo` have a dry/wet `mix`, and a reverb keeps the loudness whatever its room and mix |
| `tremolo`, `vibrato`, `auto_pan`, `orbit`, `pitch` | Periodic volume and pitch, side-to-side movement, circling around the head ("8D", with the delay and shadow of a head), pitch shift at the same speed |
| `vinyl` | The hiss and crackle of a worn record |

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
`concert_hall`, several with parameters (`bass_boost` takes `gain_db` and `frequency`). They keep
about the loudness of the music they play, apart from what they add on purpose, such as
`bass_boost`'s bass. Their nodes have readable ids, e.g. `bass_boost` splits the bass off (`low1`,
`low2`) into the `bass` compressor, whose `makeup` is the boost, and `grit` saturation, lowers the
`rest` (`high1`, `high2`) to make room, and ends in a `limiter`. `player.update` with `preset`
applies one, and the player's `graph` then shows its nodes, ready to be changed or extended and set
again as `graph`; `presets.build` returns the graph without applying it.

## Layers and clips

A player's output is a mix of *layers*. The music, which the queue plays, is one; clients add up
to four more (`layer.update`, named `a-z`, `0-9`, `_` and `-`), each with its own volume, effects
and pause, mixed *after* the music's effects: a sound on its own layer does not go through the
music's nightcore preset or its volume. Pausing the music (`player.update`) leaves the other
layers playing; a client that disconnects pauses them all, as it does the music. The sum goes
through a limiter that keeps it within full scale, so a loud clip over loud music does not clip.

Layers play *clips*: sounds outside the queue, from any source a track comes from (Discord
attachment links play through `http`), with an id the client may choose (`clipId`) and their
own events. `clip.play` loads the clip first, without holding up the player; clips sent for the
same layer still join it in the order they were sent. A layer's `mode` decides what a new clip
does while others play:

| `mode` | A new clip |
|---|---|
| `queue` (default) | Waits its turn, e.g. announcements; only the next one downloads ahead |
| `mix` | Plays over the others, up to `maxClips` (8); beyond that the oldest ends as `replaced`, e.g. a soundboard |
| `replace` | Ends the others (`replaced`) and those waiting |

Every clip that `clip.play` accepted ends exactly once with `clipEnd` (`finished`, `stopped`,
`replaced`, `failed` or `cleanup`); one that waited and was stopped never starts. A clip that
cannot be played fails on its own, with one retry, so that the ones after it play: it gets
`clipError`, or `clipStuck` after waiting for its media as long as tracks are reported stuck.

A layer with `duck` lowers others while one of its clips is heard (playing, not waiting for
media): `{"layers": ["main"], "volume": 25}` brings the music down to a quarter for an
announcement over `attack` (50 ms), holds it there through pauses of up to `hold` (250 ms), and
brings it back over `release` (500 ms). It follows the clips, not their loudness, so pauses
between words do not make the music pump. Of several layers ducking one, the lowest volume wins.

```json
{"op": "layer.update", "params": {"guildId": "1", "layer": "announcements", "duck": {"layers": ["main"], "volume": 25}}}
{"op": "clip.play", "params": {"guildId": "1", "layer": "announcements", "clipId": "a1", "track": {"query": "https://cdn.discordapp.com/attachments/.../hello.ogg"}}}
```

## Uploads

Sounds that have a URL, such as Discord attachments, play from it. For those that do not, the
engine can take uploads, if its operator turns them on with `ENTROXIMER_UPLOADS`: on a public
engine they let anyone who may use it store files there. Off, the routes answer 404.

`POST /v1/uploads` takes the file as its body, with an optional `?title=` (else the file's own
tag), and answers `201` with `{id, track, bytes, expiresAt}`. The `track` plays like any other,
in the queue or as a clip; its URI, `upload:<id>`, also loads it. `DELETE /v1/uploads/<id>`
deletes one early. Both need the password, like every route but `/health`.

- The file streams to `uploads/` in the engine's run directory, never into RAM, so uploads are
  gone after a restart. A file larger than `ENTROXIMER_UPLOAD_MAX_MB` is refused with 413
  (`too_large`), before it is read if its `Content-Length` says so.
- It is read the way it will play, by the engine's own demuxer, with its first packets decoded:
  one it cannot play is refused with 415 (`no_audio`), so an accepted upload is one that plays.
  Its tags give the title, artist, album and ISRC. It plays straight from the file, with no
  ffmpeg.
- An upload is deleted once it has gone unused (loaded or played) for
  `ENTROXIMER_UPLOAD_KEEP_MINS`; `expiresAt` says when, if it is not used before.
- All uploads together take at most `ENTROXIMER_UPLOAD_TOTAL_MB`. A new one that does not fit
  removes expired uploads, then those unused for longest; if it still does not fit, it is
  refused with 507 (`storage_full`).
- An upload that is playing is never removed. Deleting one makes it unplayable at once (loading
  it then fails with `unavailable`), and its file goes when the playing ends.

`node.info` tells clients whether uploads are on, the largest file and how long uploads are kept
(`uploads`, `null` when off). The engine warns at startup when uploads are on without a password
on an address others can reach.

## Queues

Each player plays its queue by itself. Once the current track has less than
`ENTROXIMER_PRELOAD_SECS` left, the engine resolves the next item and starts downloading it, so
it follows without a gap, whatever the client does meanwhile; if the queue changes so that
another item comes next, that one is prepared instead. Items leave the queue when they start.
Repeating a `track` plays it again from what was downloaded, without loading it again;
repeating the `queue` sends finished tracks to its end, and each is loaded again when it
comes back.

With a `crossfade`, the next track starts that long before the current one ends, fading in as
the current one fades out (at equal power, so the overlap does not dip), and the next track is
prepared that much earlier. The switch is reported when the fade starts, in the same order as
without one: `trackEnd` (`finished`), then `trackStart`. At most half of a track fades; streams,
and next tracks not downloaded far enough in time, follow without one. With a playback `rate`
the fade lasts as long and takes more of the track. Skipping, stopping and playing another track
cut at once, fading out only over a few milliseconds so that they do not click, as volume
changes do.

An item that fails to load is reported with `trackError` and removed, and the queue goes on;
after five failures in a row it waits until the client changes something. Clients mirror the
queue from `queueChanged` events and fetch it again (`queue.get`) when a version is skipped.
Queues live in memory: they survive client reconnects but not engine restarts.

Plain HTTP serves `/health`, `GET /v1/info`, `GET /v1/artwork/<key>` (embedded covers, see
[Artwork](#artwork)), `POST /v1/uploads` and `DELETE /v1/uploads/<id>` (see [Uploads](#uploads))
and `GET|PUT|DELETE /v1/youtube/session` (the cookies.txt as the `PUT` body). The protocol's types are in
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
being thrown away. A track's cache is deleted when it ends, unless it repeats: then it plays
again from it.

Besides each track's caps, `ENTROXIMER_MEMORY_TOTAL_MB` and `ENTROXIMER_DISK_TOTAL_MB` cap all
tracks together: each track playing, preparing or sounding as a clip may take an equal share,
so that a ten-hour mix on one server cannot crowd out the others. Songs take far less than
their share.

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
| `ENTROXIMER_LOCAL_FILES_DIR` | none | Let clients play the files within this directory; see [Local files](#local-files) |
| `ENTROXIMER_ALLOW_LOCAL_FILES` | off | Let clients play any file on the engine's host that it can read |
| `ENTROXIMER_ALLOW_PRIVATE_URLS` | off | Let clients play links to private networks, e.g. a media server on the LAN |
| `ENTROXIMER_UPLOADS` | off | Let clients upload audio files to play; see [Uploads](#uploads) |
| `ENTROXIMER_UPLOAD_MAX_MB` | `50` | The largest upload |
| `ENTROXIMER_UPLOAD_TOTAL_MB` | `512` | Disk all uploads may take together |
| `ENTROXIMER_UPLOAD_KEEP_MINS` | `60` | How long an upload is kept after it was last used |
| `ENTROXIMER_CACHE_DIR` | `$TMPDIR/entroximer` | Spilled packets and downloads; keep it on fast disk, not tmpfs |
| `ENTROXIMER_NO_SPILL` | off | Keep the cache in RAM only |
| `ENTROXIMER_MEMORY_BEHIND_SECS` | `15` | Seconds kept in RAM behind the playhead |
| `ENTROXIMER_FORWARD_BIAS` | `4` | How much more is kept ahead than behind |
| `ENTROXIMER_MEMORY_MAX_MB` / `ENTROXIMER_DISK_MAX_MB` | `8` / `1024` | Per-track caps |
| `ENTROXIMER_MEMORY_TOTAL_MB` / `ENTROXIMER_DISK_TOTAL_MB` | `128` / `4096` | Caps for all tracks together; each track playing, preparing or sounding as a clip may take an equal share, up to its own caps |
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

When the bot runs beside the engine, e.g. in the same pod, its sounds can be a volume mounted
into both containers at the same path, read-only in the engine's, with
`ENTROXIMER_LOCAL_FILES_DIR` set to that path: the bot then plays them by the paths it sees.

## Development

`nix develop` (or `nix-shell`) in the repository root provides the toolchain and every tool the
engine runs; elsewhere, install Rust, pkg-config, libopus, ffmpeg (with libopus), yt-dlp and Deno.

```sh
cargo test --workspace                       # needs ffmpeg with libopus for the media tests
cargo clippy --workspace --all-targets
RUST_LOG=entroximer_core=debug cargo test    # shows producer and seeking decisions
nix build                                    # the engine as a Nix package, tests included
```
