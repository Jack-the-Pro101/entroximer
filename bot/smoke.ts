// An end-to-end check in a real voice channel, without slash commands: joins, plays a track,
// seeks, pauses, applies effects, plays clips over the music on layers of their own (ducking it
// for one), plays a queue (a gapless transition, a crossfade, a SoundCloud search result,
// skipping, repeating), and leaves, checking each step against what the engine reports.
// Needs a running engine; its debug log shows when it prepares each next track.
//   bun run smoke <voice channel id> <track URL or search>
import { Client, Events, GatewayIntentBits } from "discord.js";
import { parseChain } from "entroximer-client";
import type { PlayerState, Track, TrackEndReason } from "entroximer-client";

import { connectEngine, keepGatewayReady } from "./engine";
import { env } from "./env";
import { formatTime } from "./format";

const [channelId, query] = process.argv.slice(2);
if (!channelId || !query) {
  console.error("usage: bun run smoke <voice channel id> <track URL or search>");
  process.exit(2);
}

const client = new Client({ intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates] });
const ready = new Promise<Client<true>>((resolve) => client.once(Events.ClientReady, resolve));
const gatewayReady = keepGatewayReady(client);
await client.login(env.BOT_TOKEN);
const bot = await ready;
const { manager } = await connectEngine(bot, gatewayReady);

const channel = await bot.channels.fetch(channelId);
if (!channel?.isVoiceBased()) {
  throw new Error(`${channelId} is not a voice channel the bot can see`);
}
const player = manager.player(channel.guildId);
const log: string[] = [];
player.on("trackStart", (track, itemId) => log.push(`${Date.now()} start ${track.info.title} (${itemId})`));
player.on("trackEnd", (_, reason, itemId) => log.push(`${Date.now()} end ${reason} (${itemId})`));
player.on("trackError", (_, error) => log.push(`${Date.now()} error ${error.code}: ${error.message}`));
player.on("queueEnd", () => log.push(`${Date.now()} queue end`));
player.queue.on("change", (change) => log.push(`${Date.now()} queue ${change.type}`));
// positions advance whether or not Discord takes the audio, so losing voice fails the run
const voiceLost: string[] = [];
player.on("voiceClosed", (info) => {
  log.push(`${Date.now()} voice closed: ${JSON.stringify(info)}`);
  if (info.byRemote) voiceLost.push(info.reason);
});
manager.on("error", (error) => {
  log.push(`${Date.now()} error ${error.code}: ${error.message}`);
  if (error.code === "voice_conflict") voiceLost.push(error.message);
});

/** The engine's current state of the player. */
async function state(): Promise<PlayerState> {
  return (await player.update({})).state;
}

/** Polls the player until `done` holds; returns how long it took. */
async function waitFor(what: string, done: (state: PlayerState) => boolean, timeoutMs: number): Promise<number> {
  const started = Date.now();
  for (;;) {
    if (done(await state())) {
      return Date.now() - started;
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await Bun.sleep(250);
  }
}

function nextEvent<E extends "trackStart" | "trackEnd" | "queueEnd">(event: E): Promise<number> {
  return new Promise((resolve) => player.once(event, () => resolve(Date.now())));
}

/** When the next track starts, and its queue item. */
function nextStart(): Promise<{ at: number; itemId: string }> {
  return new Promise((resolve) => player.once("trackStart", (_, itemId) => resolve({ at: Date.now(), itemId })));
}

/** When the current track ends, and why. */
function nextEnd(): Promise<{ at: number; reason: TrackEndReason }> {
  return new Promise((resolve) => player.once("trackEnd", (_, reason) => resolve({ at: Date.now(), reason })));
}

const results: Array<{ step: string; ok: boolean; detail: string }> = [];
async function step(name: string, run: () => Promise<string>): Promise<void> {
  try {
    const detail = await run();
    results.push({ step: name, ok: true, detail });
    console.log(`✓ ${name}: ${detail}`);
  } catch (error) {
    results.push({ step: name, ok: false, detail: String(error) });
    console.log(`✗ ${name}: ${error}`);
  }
}

let track: Track | undefined;

await step("join the voice channel", async () => {
  player.connect(channel.id);
  const took = await waitFor("the voice connection", (s) => s.connected, 20_000);
  return `connected to #${channel.name} in ${took} ms`;
});

await step("load", async () => {
  const started = Date.now();
  const loaded = await manager.load(query);
  track =
    loaded.type === "track"
      ? loaded.track
      : loaded.type === "playlist"
        ? loaded.tracks[loaded.playlist.selectedTrack ?? 0]
        : loaded.type === "search"
          ? loaded.tracks[0]
          : undefined;
  if (!track) throw new Error(`nothing found (${loaded.type})`);
  const { title, length, author, artworkUrl, sourceName } = track.info;
  const took = Date.now() - started;
  return `${title} by ${author ?? "?"} (${length === null ? "?" : formatTime(length)}, ${sourceName}, artwork ${artworkUrl ? "yes" : "no"}) in ${took} ms`;
});

await step("play", async () => {
  if (!track) throw new Error("nothing loaded");
  const started = Date.now();
  const startEvent = nextEvent("trackStart");
  await player.play(track);
  await startEvent;
  await waitFor("audio", (s) => s.position > 0, 20_000);
  const firstAudio = Date.now() - started;
  await waitFor("5 s of playback", (s) => s.position >= 5_000, 20_000);
  return `audio ${firstAudio} ms after the request; position advancing`;
});

await step("seek ahead of the download", async () => {
  const started = Date.now();
  await player.seek(60_000);
  await waitFor("playback after 1:00", (s) => s.position >= 61_000, 20_000);
  return `playing at ${formatTime((await state()).position)} after ${Date.now() - started} ms`;
});

await step("seek back into downloaded audio", async () => {
  await player.seek(10_000);
  await Bun.sleep(1_500);
  const { position } = await state();
  if (position < 10_000 || position > 13_000) throw new Error(`position ${position} ms`);
  return `at ${formatTime(position)}`;
});

await step("pause holds the position", async () => {
  await player.pause();
  const before = (await state()).position;
  await Bun.sleep(2_000);
  const after = (await state()).position;
  if (Math.abs(after - before) > 100) throw new Error(`moved from ${before} to ${after} ms while paused`);
  await player.resume();
  await waitFor("playback after resuming", (s) => s.position >= after + 1_000, 10_000);
  return `held at ${formatTime(before)}, continued after resuming`;
});

await step("effects", async () => {
  const { nodes } = await manager.info();
  await player.setGraph(parseChain("lowpass(800) => reverb(10, 2)", nodes));
  await Bun.sleep(4_000);
  await player.setGraph(null);
  await player.setVolume(40);
  await Bun.sleep(3_000);
  await player.setVolume(100);
  return "lowpass and reverb for 4 s, then 40% volume for 3 s";
});

await step("layers: a clip plays over the music, and on while the music is paused", async () => {
  if (!track) throw new Error("nothing loaded");
  const sfx = player.layer("smoke-sfx");
  await sfx.configure({ mode: "mix", volume: 60 });
  const started = new Promise<void>((resolve) => sfx.once("clipStart", () => resolve()));
  const ended = new Promise<TrackEndReason>((resolve) => sfx.once("clipEnd", (_, reason) => resolve(reason)));
  // a stretch of the same track, from 1:00 to 1:06
  const clip = await sfx.play(track, { startAt: 60_000, endAt: 66_000, fadeIn: 300 });
  await started;
  await Bun.sleep(2_000);
  await player.pause();
  const held = (await state()).position;
  const reason = await ended;
  const after = (await state()).position;
  await player.resume();
  await sfx.remove();
  if (reason !== "finished") throw new Error(`the clip ended with ${reason}`);
  if (Math.abs(after - held) > 100) throw new Error(`the paused music moved from ${held} to ${after} ms`);
  return `${clip.status} at once; it finished while the music held at ${formatTime(held)}`;
});

await step("layers: an announcement lowers the music while it plays", async () => {
  if (!track) throw new Error("nothing loaded");
  const announcements = player.layer("smoke-announcements");
  await announcements.setDuck({ layers: ["main"], volume: 20 });
  const ended = new Promise<TrackEndReason>((resolve) =>
    announcements.once("clipEnd", (_, reason) => resolve(reason)),
  );
  await announcements.play(track, { startAt: 30_000, endAt: 36_000 });
  const reason = await ended;
  await announcements.remove();
  if (reason !== "finished") throw new Error(`the announcement ended with ${reason}`);
  return "6 s of the track at 0:30 over the music, lowered to 20%";
});

await step("queue: the next track loads ahead and follows without a gap", async () => {
  if (!track) throw new Error("nothing loaded");
  const added = await player.queue.add(track);
  const itemId = added.items[0]?.id;
  // long enough that the engine prepares the next track ahead (30 s by default), as its log shows
  const { position } = await state();
  const ended = nextEnd();
  const started = nextStart();
  await player.setEndTime(position + 35_000);
  const end = await ended;
  const start = await started;
  if (end.reason !== "finished") throw new Error(`the first track ended with ${end.reason}`);
  if (start.itemId !== itemId) throw new Error(`started item ${start.itemId}, not the queued ${itemId}`);
  if (player.queue.length !== 0) throw new Error(`${player.queue.length} items left in the queue`);
  await waitFor("the next track to play", (s) => s.position >= 2_000, 15_000);
  return `switched with ${start.at - end.at} ms between the events; the queued item plays`;
});

await step("queue: a crossfade starts the next track before the current one ends", async () => {
  if (!track) throw new Error("nothing loaded");
  await player.queue.setCrossfade(6_000);
  const added = await player.queue.add(track);
  const itemId = added.items[0]?.id;
  // the current track ends in 40 s, so the next one starts over it in 34 s
  const { position } = await state();
  const requested = Date.now();
  const ended = nextEnd();
  const started = nextStart();
  await player.setEndTime(position + 40_000);
  const end = await ended;
  const start = await started;
  await player.queue.setCrossfade(0);
  if (end.reason !== "finished") throw new Error(`the first track ended with ${end.reason}`);
  if (start.itemId !== itemId) throw new Error(`started item ${start.itemId}, not the queued ${itemId}`);
  const early = 40_000 - (start.at - requested);
  if (Math.abs(early - 6_000) > 1_500) throw new Error(`the next track started ${early} ms before the end`);
  await waitFor("the next track to play", (s) => s.position >= 8_000, 15_000);
  return `the next track started ${early} ms before the end of the first`;
});

await step("queue: a SoundCloud search result, skipped to and seeked", async () => {
  const { tracks } = await manager.search("fallen kingdom", { prefix: "scsearch", limit: 1 });
  const found = tracks[0];
  if (!found) throw new Error("the search found nothing");
  await player.queue.add(found);
  const requested = Date.now();
  const started = nextStart();
  await player.skip();
  await started;
  await waitFor("audio", (s) => s.position > 0, 20_000);
  const firstAudio = Date.now() - requested;
  await player.seek(60_000);
  await waitFor("playback after 1:00", (s) => s.position >= 61_000, 20_000);
  const info = player.track?.info;
  return `${info?.title} by ${info?.author} (${info?.sourceName}): audio ${firstAudio} ms after the skip, seeked to 1:00`;
});

await step("queue: repeating a track", async () => {
  await player.queue.setRepeat("track");
  const itemId = player.itemId;
  const { position } = await state();
  const ended = nextEnd();
  const started = nextStart();
  await player.setEndTime(position + 3_000);
  const end = await ended;
  const start = await started;
  if (end.reason !== "finished") throw new Error(`the track ended with ${end.reason}`);
  if (start.itemId !== itemId) throw new Error(`started item ${start.itemId}, not ${itemId} again`);
  await player.queue.setRepeat("off");
  return `the same item started again ${start.at - end.at} ms after it ended`;
});

await step("queue: skipping past the end", async () => {
  const ended = nextEvent("queueEnd");
  await player.skip();
  await ended;
  if (player.track !== null) throw new Error("a track still plays");
  return "the queue ended and playback stopped";
});

await step("leave", async () => {
  const destroyed = new Promise<void>((resolve) => player.once("destroyed", resolve));
  await player.destroy();
  await destroyed;
  return "left the voice channel";
});

results.push(
  voiceLost.length === 0
    ? { step: "voice stayed connected", ok: true, detail: "" }
    : { step: "voice stayed connected", ok: false, detail: voiceLost.join("; ") },
);
if (voiceLost.length > 0) console.log(`✗ voice stayed connected: ${voiceLost.join("; ")}`);

console.log(`\nevents:\n${log.map((line) => `  ${line}`).join("\n")}`);
const failed = results.filter((result) => !result.ok).length;
console.log(`\n${results.length - failed}/${results.length} steps passed`);
manager.close();
await bot.destroy();
process.exit(failed === 0 ? 0 : 1);
