# entroximer-client

TypeScript client for the Entroximer engine. It works with any Discord library: it only needs
a way to send a payload on the bot's gateway, and the gateway's raw voice events. It runs on
Bun, whose `WebSocket` can send the headers the engine expects.

```ts
import { Client, Events, GatewayIntentBits } from "discord.js";
import { Entroximer } from "entroximer-client";

const client = new Client({ intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates] });

client.once(Events.ClientReady, async (ready) => {
  const engine = new Entroximer({
    url: "ws://127.0.0.1:2333",
    password: process.env.ENTROXIMER_PASSWORD, // if the engine has one
    userId: ready.user.id,
    sendToGateway: (guildId, payload) => ready.guilds.cache.get(guildId)?.shard.send(payload),
  });
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
| `destroy()` | Stops playback, leaves the voice channel and removes the player |

A player keeps the engine's view of `track` (and its queue `itemId`), `paused`, `volume`, `graph`
and `state` (buffered ranges, connection, buffering), and estimates `position` between updates.
It emits `trackStart`, `trackEnd`, `trackError`, `trackStuck` (each with the track's item id),
`queueEnd`, `update`, `voiceClosed`, `error` and `destroyed`. Leaving the voice channel in
Discord destroys the player as well.

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

## Queues

Each player has a `queue`, which the engine plays by itself: it loads the next track well
before the current one ends (`ENTROXIMER_PRELOAD_SECS`, 30 s by default), so tracks follow each
other without a gap.

| Method | Does |
|---|---|
| `add(tracks, { index, userData })` | Adds tracks or queries: a search adds its first result, a playlist link all its tracks |
| `remove(...ids)`, `move(id, index)`, `clear()`, `shuffle()` | Edit the queue |
| `setRepeat("off" \| "track" \| "queue")` | Repeat the current track, or send finished tracks to the end |

`queue.items` mirrors the engine's queue, following its changes (`change` events); if any are
missed, the queue fetches itself again (`sync`). Search results and playlist tracks carry the
metadata their listing had; once the engine loads one to play it, fuller metadata (e.g. a
YouTube Music result's artist) arrives as an `updated` change. A queued track that fails to
load is reported with `trackError` and removed, and the queue goes on.

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
The protocol's types in `src/generated` are produced from the engine by `cargo test`.

## Development

```sh
bun test            # integration tests start the engine built by `cargo build` in ../engine
bun run typecheck
```
