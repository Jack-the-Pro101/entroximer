# entroximer-client

TypeScript client for the Entroximer engine. It works with any Discord library: it only needs
a way to send a payload on the bot's gateway, and the gateway's raw voice events. It runs on
Bun and on Node 22 or later, whose `WebSocket` can send the headers the engine expects (a
browser's cannot).

```ts
import { Client, Events, GatewayIntentBits } from "discord.js";
import { Entroximer } from "entroximer-client";
import type { GatewayDispatch } from "entroximer-client";

const client = new Client({ intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates] });
// READY comes before `clientReady`; keep it for the engine's client (see below)
const gatewayReady: GatewayDispatch[] = [];
client.on(Events.Raw, (packet) => packet.t === "READY" && gatewayReady.push(packet));

client.once(Events.ClientReady, async (ready) => {
  const engine = new Entroximer({
    url: "ws://127.0.0.1:2333",
    password: process.env.ENTROXIMER_PASSWORD, // if the engine has one
    userId: ready.user.id,
    sendToGateway: (guildId, payload) => ready.guilds.cache.get(guildId)?.shard.send(payload),
  });
  for (const packet of gatewayReady) engine.handleRawEvent(packet);
  ready.on(Events.Raw, (packet) => engine.handleRawEvent(packet));
  await engine.connect();

  const player = engine.player(guildId);
  player.connect(voiceChannelId);
  player.on("trackStart", (track) => console.log(`now playing ${track.info.title}`));
  // with nothing playing, the first track of the queue starts
  await player.queue.add("https://www.youtube.com/watch?v=I-sH53vXP2A");
});
```

## Installing

The package is JavaScript compiled to `dist/`, with type declarations. In this repository's
workspace, depend on it with `"entroximer-client": "workspace:*"`; elsewhere, install the tarball
`bun pm pack` makes, which builds it first:

```sh
(cd client && bun pm pack)                  # entroximer-client-0.1.0.tgz
bun add ./path/to/entroximer-client-0.1.0.tgz
```

## Players

`engine.player(guildId)` returns the guild's `Player`; the engine creates its side on the first
change.

| Method | Does |
|---|---|
| `connect(channelId)` | Joins (or moves to) a voice channel through the gateway |
| `play(track, { startAt, endAt, userData, onlyIfIdle })` | Plays a track now, in place of the current one; the queue goes on afterwards. `track` is a loaded `Track` or a URL/search to load; `startAt` and `endAt` replace the link's or the track's own times (see [Start and end times](#start-and-end-times)) |
| `skip(toItem?)` | Plays the queue's next item (or `toItem`, dropping the ones before it); stops if the queue is empty |
| `seek(ms)`, `pause()`, `resume()` | Control playback |
| `stop()` | Stops playback; the queue waits until something plays again |
| `setVolume(percent, { clipping? })`, `setEndTime(ms)` | 100 % leaves the audio unchanged, up to 1000 % |
| `setClipping(clipping)` | Hard clips the audio after the volume, so that above 100 % it distorts, for fun, instead of the engine's limiter holding it at full scale |
| `setTimes(itemId, { startTime, endTime })` | Changes the part of a queued or the playing track (`itemId`) that plays; see [Start and end times](#start-and-end-times) |
| `setPreset(name, params?)` | Effects from one of the engine's presets, e.g. `setPreset("nightcore")` or `setPreset("bass_boost", { gain_db: 9 })` |
| `setGraph(graph)` | Effects, e.g. `chain({ type: "lowpass", params: { cutoff: 800 } })` or `parseChain("lowpass(800) => reverb", info.nodes)`; `null` removes them |
| `update(changes)` | Several changes in one request |
| `view({ collapse? })` | The signal flow laid out for drawing, one node a row with box-drawing wires: each layer's source, effects and volume into the mix (see the engine's [Viewing the signal flow](../engine/README.md#viewing-the-signal-flow)). Effects are named by `layer`, `id` and `graphTag` |
| `setTextChannel(channelId)` | Where the bot posts about this player (`textChannelId`); `null` for nowhere |
| `destroy()` | Stops playback, leaves the voice channel and removes the player |

A player keeps the engine's view of `track` (and its queue `itemId`), `paused`, `volume`,
`clipping`, `graph` and `state` (buffered ranges, connection, buffering), and estimates `position` between updates.
It emits `trackStart`, `trackEnd`, `trackError`, `trackStuck` (each with the track's item id),
`queueEnd`, `update`, `voiceClosed`, `textChannelChange`, `error` and `destroyed`; its errors
reach the client's `error` event too, with the guild ID. Leaving the voice channel in Discord
destroys the player as well.

A player's `textChannelId` is for the bot: the client only keeps it, never the engine, and it
lasts as long as the player, so the next player in the guild starts without one. It stays
readable once the player is destroyed, e.g. to say in `playersLost` that playback stopped.

Discord sends the bot's voice states from every gateway session to every session. Given `READY`,
the client knows its own sessions and ignores the others' voice states: when another instance of
the bot (or another shard process) joins voice in a guild, which ends this one's voice connection
with close code 4006, the player reports a `voice_conflict` error instead of taking the
connection back. Without `READY` it cannot tell, and both instances keep replacing each other's
voice sessions: no audio, and `voiceClosed` with code 4006.

## Effects

The engine runs a player's effects as a graph of nodes (`info().nodes` lists their types) with an
optional playback `rate`; `info().presets` lists ready-made graphs. A preset is a starting point:
after `setPreset`, `player.graph` holds the nodes it built, under readable ids, to change and set
again. The helpers in `graph.ts` return new graphs and never change the one given.

```ts
await player.setPreset("bass_boost");
// more bass from its compressor's makeup gain, then a small room after everything the preset does
await player.setGraph(append(withParams(player.graph!, "bass", { makeup: 9 }), { type: "reverb", params: { mix: 0.2 } }));

// or build one first, without applying it: a phone line, played a little faster
const graph = withRate(await engine.buildPreset("telephone"), 1.15);
```

| Helper | Returns |
|---|---|
| `chain(...effects)`, `parseChain(text, info.nodes)` | A chain of effects, from objects or from `lowpass(800) => reverb(10, 2)` |
| `append(graph, ...effects)`, `prepend(graph, ...effects)` | The graph with effects after (or before) everything it does, rewiring its edges |
| `withParams(graph, id, params)` | The graph with one node's parameters changed |
| `withRate(graph, rate)` | The graph at a playback rate from 0.5 to 2 (1 removes it) |

## Layers

Besides the music, a player has up to four *layers* of its own sounds, *clips*, mixed over the
music after its effects: an announcement does not go through the music's nightcore preset or
its volume. `player.layer(name)` returns the `Layer`; the engine makes it with its first change
or clip, and `player.layers` holds the ones the engine reported or this client used.

```ts
const announcements = player.layer("announcements");
// one after another, each lowering the music to a quarter while it plays
await announcements.setDuck({ layers: ["main"], volume: 25 });
await announcements.play(attachment.url, { userData: { by: user.id } });

const sfx = player.layer("sfx");
await sfx.configure({ mode: "mix", volume: 80 });
await sfx.play("https://example.com/rain.ogg", { loop: true, fadeIn: 2_000 });
```

| Method | Does |
|---|---|
| `play(track, { clipId, volume, startAt, endAt, fadeIn, fadeOut, loop, userData })` | Plays a sound on the layer; resolves with its `ClipInfo` (`playing`, or `waiting` its turn) |
| `stop(clipId?, fadeOut?)` | Stops a clip, or every clip of the layer, waiting ones too |
| `configure(changes)` | Several changes in one request: `volume`, `clipping`, `graph` or `preset`, `paused`, `mode`, `maxClips`, `duck` |
| `setVolume(percent, { clipping? })`, `setClipping(clipping)`, `setGraph(graph)`, `setPreset(name, params?)`, `pause()`, `resume()` | As for the player, for this layer alone |
| `setDuck({ layers, volume, attack, release, hold })` | Lowers other layers (`main` is the music) while this one's clips are heard; `null` stops |
| `remove()` | Removes the layer, ending its clips |

A layer's `mode` decides what a clip does while others play: `queue` (the default) waits its
turn, `mix` plays over them (up to `maxClips`, 8; the oldest makes room), `replace` ends them.
Pausing the player pauses only the music; layers pause on their own.

A layer mirrors the engine's `mode`, `volume`, `clipping`, `paused`, `graph`, `duck` and `clips`, and emits
`clipStart`, `clipEnd` (with a reason), `clipError` and `clipStuck`, each with the clip's id;
the player emits them too, with the layer's name. Clip events never touch `player.track` or the
queue. Every clip `play` resolved ends once with `clipEnd`; one that waited and was stopped never
starts. `play` sends a `clipId` it makes up if none is given, so listeners can tell a clip's
events apart even when they come before `play` resolves: a clip that starts right away does so
before.

Sounds with a URL, such as Discord attachments, play from it. For ones without, an engine whose
operator allows it (`info().uploads`, `null` if not) takes uploads:

```ts
// on Node, `await fs.openAsBlob("doorbell.ogg")`
const upload = await manager.upload(Bun.file("doorbell.ogg"), { title: "Doorbell" });
await player.layer("sfx").play(upload.track);
// later, or let it expire: it is kept for `info().uploads.keep` after it was last used
await manager.deleteUpload(upload.id);
```

`upload` takes a `Blob`, `ArrayBuffer` or `Uint8Array` and fails with `too_large` above
`uploads.maxBytes` (before sending anything), `no_audio` for a file the engine cannot play, and
`storage_full` while every upload the engine keeps is playing; otherwise the engine makes room by
removing the uploads unused for longest. Its `track` plays like any other, as a clip or in the
queue, until the upload is deleted or expires; then loading it fails with `unavailable`.

## Queues

Each player has a `queue`, which the engine plays by itself: it loads the next track well
before the current one ends (`ENTROXIMER_PRELOAD_SECS`, 30 s by default), so tracks follow each
other without a gap.

| Method | Does |
|---|---|
| `add(tracks, { index, userData, startIndex, startTime, endTime })` | Adds loaded tracks, or queries, which load in the background: a search adds its first result, a playlist link its tracks (from the one at `startIndex`, counted from 0, if given). Each plays from its `startTime` to its `endTime`: the options', else its link's or its own (see [Start and end times](#start-and-end-times)) |
| `whenLoaded(id)` | Resolves once a loading item has loaded, with the items in its place (and the `playlist` they came from); rejects with its error |
| `remove(...ids)`, `move(id, index)`, `clear()`, `shuffle()` | Edit the queue |
| `setRepeat("off" \| "track" \| "queue")` | Repeat the current track, or send finished tracks to the end |
| `setCrossfade(ms)` | Fade each track into the next over `ms` (up to 12 000; `queue.crossfade`); 0 plays them back to back |

`queue.items` mirrors the engine's queue, following its changes (`change` events); if any are
missed, the queue fetches itself again (`sync`). Search results and playlist tracks carry the
metadata their listing had; once the engine loads one to play it, fuller metadata (e.g. a
YouTube Music result's artist) arrives as an `updated` change. A queued track that fails to
load is reported with `trackError` and removed, and the queue goes on.

`add` answers at once, also for a long list of links: each query becomes a *loading* item in
its place (`item.loading`; `queue.loading` counts them), whose `track` stands in for it with the
link or search as its title and `uri` and `sourceName` `query`. The engine loads them in queue
order, the first ahead of the rest, so playing starts as soon as the first is ready, and
replaces each with what it found (a `loaded` change); the first of those keeps the item's id. A
query added on its own loads at once, as `load` would load it, so `add` then `whenLoaded` takes
no longer than `load` then `add` did.
Loading items can be moved, removed and skipped to like any other; a skip to one waits until it
has loaded. A query that finds nothing is reported with `trackError`
and removed.

```ts
// links from several sites, one per line; blank lines and `#` comments are left out
const added = await player.queue.add(parseQueries(text), { userData: { by: user.id } });
const { items, playlist } = await player.queue.whenLoaded(added.items[0]!.id);
```

### Start and end times

A track can play only part of itself: from its `startTime` to its `endTime` (milliseconds, on
`Track` beside `userData`; `info.length` stays the whole track's). The engine plays that part
every time the track plays, also when repeated, and its `trackEnd` is `finished` at the end time.
The engine never reads times from links, so the client does: `queue.add`, `play`, `layer.play`
and `load` take a link's times out of it with `parseLinkTimes` and send them along, and a `Track`
keeps its own when queued again. Given times replace both: `{ startTime: 0 }` (or `null`) plays
from the beginning whatever the link says, `{ endTime: null }` to the end.

`player.setTimes(itemId, { startTime, endTime })` changes them later, for an item in the queue
(it arrives as an `updated` change) or the playing one (`player.itemId`; `player.track` shows
them); a time left out stays, `null` clears it. The playing track plays on from where it is: a new
start applies the next time it plays (repeated, or in the queue's next round), a new end right
away, and an end it has passed ends it now.

| Link | Gives |
|---|---|
| YouTube (`youtube.com`, `youtu.be`, YouTube Music, embeds) | `t=90`, `t=1m30s`, `start=`, `time_continue=` as the start and `end=` as the end, in the query or the fragment |
| Twitch videos (`twitch.tv/videos/...`) | `t=1h2m3s` |
| Dailymotion | `start=90` |
| bilibili | `t=90` |
| Any other link, e.g. SoundCloud, Vimeo, a media file | A media fragment: `#t=90`, `#t=1:23`, `#t=1m30s`, `#t=10,20` (from and to) or `#t=,20` (to) |

Other query parameters named `t` (often cache busters) are left alone. `parseDuration(text)`
reads the times these use, and what people type: `90`, `90.5`, `90s`, `1m30s`, `1h2m3s`, `1:30` or
`1:02:03`, in milliseconds (`null` if it is none of these).

```ts
parseLinkTimes("https://youtu.be/I-sH53vXP2A?t=1m30s");
// { query: "https://youtu.be/I-sH53vXP2A", startTime: 90000 }
await player.queue.add("https://youtu.be/I-sH53vXP2A?t=1m30s", { endTime: parseDuration("2:45")! });
```

With a crossfade, the next track's `trackStart` comes when the fade starts, right after the
current track's `trackEnd` (`finished`), so `player.track` shows the incoming track while the
other fades out under it. A crossfade is no change to the queue's items, so it has no `change`
event: `queue.crossfade` is what this client last set.

## Connection

`Entroximer` also loads tracks and reports `info` and `stats`. `load(query)` returns a
`track`, a `playlist` (with `selectedTrack` for links to a track within it), `search` results
or `empty`; `search(query, { prefix, limit })` searches one source, by a prefix from
`info().providers` (`ytsearch`, `ytmsearch`, `scsearch`, and `spsearch` if the engine has a
Spotify app; Spotify tracks play from YouTube); `decode` reads `encoded` tracks back;
`buildPreset(name, params?)` returns a preset's graph without applying it. Tracks carry
`info.artworkUrl`; `artwork(track)` says where it is and whether anyone can fetch it, and
`fetchArtwork(track)` returns the image. Covers the engine extracted from media files are on the
engine, so Discord cannot load them from their URL: upload the image with the message instead
(the bot in `../bot` shows how). When the
connection drops, it reconnects and resumes the session: the engine pauses the players
meanwhile and continues them where they were. If it reconnects after the engine's resume
timeout, or after the engine restarted, `playersLost` lists the players that are gone; the bot
leaves their voice channels. `youtubeSession` reports when the
engine's YouTube session needs fresh cookies.

Failures are `EntroximerError`s with a `code`: the engine's codes (`age_restricted`,
`unavailable`, `bad_request`, ...) or the client's own (`connection_lost`, `timeout`, `closed`).
A request times out after `requestTimeoutMs` (30 s), or `loadTimeoutMs` (150 s) for those that
may load tracks (`load`, `search`, adding to the queue, playing), which take as long as the source
does; both include waiting for a dropped connection to come back. Times and percents may be
fractional; they are rounded to the whole milliseconds and percents the engine takes.
The protocol's types in `src/generated` are produced from the engine by `cargo test`.

## Development

```sh
bun test            # integration tests start the engine built by `cargo build` in ../engine
bun run typecheck
bun run build       # dist/; packing builds it by itself
```

Imports within the package name their files with `.js` (`./player.js`), as Node's ES modules
need; TypeScript and Bun find the `.ts` source behind them.

Packages in the workspace can use the TypeScript source instead of `dist/`, so that changes need
no build: the `entroximer-source` export condition points at it. The bot sets it for TypeScript
(`"customConditions": ["entroximer-source"]` in its `tsconfig.json`) and for Bun (`bun run
--conditions=entroximer-source`).
