# entroximer-client

TypeScript client for the Entroximer engine. It works with any Discord library: it only needs
a way to send a payload on the bot's gateway, and the gateway's raw voice events. It runs on
Bun, whose `WebSocket` can send the headers the engine expects.

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

## Players

`engine.player(guildId)` returns the guild's `Player`; the engine creates its side on the first
change.

| Method | Does |
|---|---|
| `connect(channelId)` | Joins (or moves to) a voice channel through the gateway |
| `play(track, { startAt, endAt, userData, onlyIfIdle })` | Plays a track now, in place of the current one; the queue goes on afterwards. `track` is a loaded `Track` or a URL/search to load |
| `skip(toItem?)` | Plays the queue's next item (or `toItem`, dropping the ones before it); stops if the queue is empty |
| `seek(ms)`, `pause()`, `resume()` | Control playback |
| `stop()` | Stops playback; the queue waits until something plays again |
| `setVolume(percent)`, `setEndTime(ms)` | 100 % leaves the audio unchanged, up to 1000 % |
| `setPreset(name, params?)` | Effects from one of the engine's presets, e.g. `setPreset("nightcore")` or `setPreset("bass_boost", { gain_db: 9 })` |
| `setGraph(graph)` | Effects, e.g. `chain({ type: "lowpass", params: { cutoff: 800 } })` or `parseChain("lowpass(800) => reverb", info.nodes)`; `null` removes them |
| `update(changes)` | Several changes in one request |
| `setTextChannel(channelId)` | Where the bot posts about this player (`textChannelId`); `null` for nowhere |
| `destroy()` | Stops playback, leaves the voice channel and removes the player |

A player keeps the engine's view of `track` (and its queue `itemId`), `paused`, `volume`, `graph`
and `state` (buffered ranges, connection, buffering), and estimates `position` between updates.
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
// more bass, then a small room after everything the preset does
await player.setGraph(append(withParams(player.graph!, "bass", { gain_db: 9 }), { type: "reverb", params: { mix: 0.2 } }));

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
| `configure(changes)` | Several changes in one request: `volume`, `graph` or `preset`, `paused`, `mode`, `maxClips`, `duck` |
| `setVolume(percent)`, `setGraph(graph)`, `setPreset(name, params?)`, `pause()`, `resume()` | As for the player, for this layer alone |
| `setDuck({ layers, volume, attack, release, hold })` | Lowers other layers (`main` is the music) while this one's clips are heard; `null` stops |
| `remove()` | Removes the layer, ending its clips |

A layer's `mode` decides what a clip does while others play: `queue` (the default) waits its
turn, `mix` plays over them (up to `maxClips`, 8; the oldest makes room), `replace` ends them.
Pausing the player pauses only the music; layers pause on their own.

A layer mirrors the engine's `mode`, `volume`, `paused`, `graph`, `duck` and `clips`, and emits
`clipStart`, `clipEnd` (with a reason), `clipError` and `clipStuck`, each with the clip's id;
the player emits them too, with the layer's name. Clip events never touch `player.track` or the
queue. Every clip `play` resolved ends once with `clipEnd`; one that waited and was stopped never
starts. `play` sends a `clipId` it makes up if none is given, so listeners can tell a clip's
events apart even when they come before `play` resolves: a clip that starts right away does so
before.

Sounds with a URL, such as Discord attachments, play from it. For ones without, an engine whose
operator allows it (`info().uploads`, `null` if not) takes uploads:

```ts
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
| `add(tracks, { index, userData })` | Adds tracks or queries: a search adds its first result, a playlist link all its tracks |
| `remove(...ids)`, `move(id, index)`, `clear()`, `shuffle()` | Edit the queue |
| `setRepeat("off" \| "track" \| "queue")` | Repeat the current track, or send finished tracks to the end |
| `setCrossfade(ms)` | Fade each track into the next over `ms` (up to 12 000; `queue.crossfade`); 0 plays them back to back |

`queue.items` mirrors the engine's queue, following its changes (`change` events); if any are
missed, the queue fetches itself again (`sync`). Search results and playlist tracks carry the
metadata their listing had; once the engine loads one to play it, fuller metadata (e.g. a
YouTube Music result's artist) arrives as an `updated` change. A queued track that fails to
load is reported with `trackError` and removed, and the queue goes on.

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
```
