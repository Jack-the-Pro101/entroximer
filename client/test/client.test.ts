import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Entroximer, EntroximerError, PROTOCOL_VERSION, append, withParams } from "../src/index.js";
import type {
  EntroximerOptions,
  GatewayVoiceStateUpdate,
  Track,
  TrackEndReason,
} from "../src/index.js";
import { canRunEngine, nextCall, startEngine } from "./engine.js";
import type { TestEngine } from "./engine.js";

const BOT = "1000";
const PASSWORD = "test password";

describe.skipIf(!canRunEngine)("against a real engine", () => {
  let engine: TestEngine;
  const clients: Entroximer[] = [];

  beforeAll(async () => {
    engine = await startEngine({ password: PASSWORD, uploads: true });
  });

  afterAll(async () => {
    for (const client of clients) {
      client.close();
    }
    await engine.stop();
  });

  function client(options: Partial<EntroximerOptions> = {}): Entroximer {
    const created = new Entroximer({
      url: engine.url,
      userId: BOT,
      password: PASSWORD,
      sendToGateway: () => {},
      ...options,
    });
    clients.push(created);
    return created;
  }

  async function loadTrack(manager: Entroximer, query: string): Promise<Track> {
    const loaded = await manager.load(query);
    if (loaded.type !== "track") {
      throw new Error(`expected a track, got ${loaded.type}`);
    }
    return loaded.track;
  }

  /** Waits for a `failed` call to an async function and returns its error. */
  async function failure(promise: Promise<unknown>): Promise<EntroximerError> {
    const error = await promise.then(
      () => {
        throw new Error("expected a failure");
      },
      (e: unknown) => e,
    );
    expect(error).toBeInstanceOf(EntroximerError);
    return error as EntroximerError;
  }

  test("connects and describes the engine", async () => {
    const info = await client().connect();
    expect(info.protocol).toBe(PROTOCOL_VERSION);
    expect(info.nodes.map((node) => node.type)).toContain("reverb");
    const youtube = info.providers.find((provider) => provider.name === "youtube");
    expect(youtube?.searchPrefixes).toEqual(["ytsearch", "ytmsearch"]);
  });

  test("connecting again waits for the same connection", async () => {
    const manager = client();
    const [first, second] = await Promise.all([manager.connection.connect(), manager.connection.connect()]);
    expect(second.sessionId).toBe(first.sessionId);
    expect((await manager.connection.connect()).sessionId).toBe(first.sessionId);
  });

  test("takes fractional milliseconds and percents", async () => {
    const manager = client();
    await manager.connect();
    const player = manager.player("7");
    expect((await player.setVolume(55.4)).volume).toBe(55);
    expect((await player.queue.setCrossfade(1500.7)).crossfade).toBe(1501);
    const clip = await player.layer("sfx").play(engine.tone, { volume: 80.2, fadeIn: 10.5 });
    expect(clip.status).toBe("playing");
    await player.destroy();
  });

  test("destroying while the connection drops leaves no player behind", async () => {
    const manager = client();
    await manager.connect();
    await manager.player("8").setVolume(50);
    // the engine may or may not get the request before the connection closes; if it does not,
    // the client destroys the player again once it has reconnected
    manager.connection.reconnect();
    await manager.player("8").destroy();
    const deadline = Date.now() + 10_000;
    for (;;) {
      const error = await manager.connection
        .request("player.get", { guildId: "8" })
        .then(() => null, (e: unknown) => e as EntroximerError);
      if (error?.code === "not_found") {
        break;
      }
      expect(Date.now()).toBeLessThan(deadline);
      await Bun.sleep(50);
    }
  });

  test("a wrong password is refused", async () => {
    const error = await client({ password: "wrong" }).connect().catch((e: unknown) => e);
    expect(error).toBeInstanceOf(EntroximerError);
    expect((error as EntroximerError).code).toBe("connection_lost");
  });

  test("plays, changes and stops a track, reporting events", async () => {
    const manager = client();
    await manager.connect();
    const track = await loadTrack(manager, engine.tone);
    // an untagged file is named after itself
    expect(track.info.title).toBe("tone");
    expect(track.info.sourceName).toBe("local");

    const player = manager.player("1");
    const started = nextCall<[Track, string]>();
    player.on("trackStart", started.listener);
    const info = await player.play(track, { userData: { requester: "42" } });
    expect(info.track?.info.title).toBe("tone");
    const [startedTrack, itemId] = await started.called;
    expect(startedTrack.userData).toEqual({ requester: "42" });
    expect(player.track?.encoded).toBe(track.encoded);
    expect(player.itemId).toBe(itemId);

    await player.setVolume(50);
    expect(player.volume).toBe(50);
    await player.pause();
    expect(player.paused).toBe(true);
    await player.seek(5_000);

    const ended = nextCall<[Track, TrackEndReason]>();
    player.on("trackEnd", ended.listener);
    await player.stop();
    expect((await ended.called)[1]).toBe("stopped");
    expect(player.track).toBeNull();

    const destroyed = nextCall<[]>();
    player.on("destroyed", destroyed.listener);
    await player.destroy();
    await destroyed.called;
    expect(manager.players.has("1")).toBe(false);
  });

  test("applies presets and builds on them", async () => {
    const manager = client();
    const info = await manager.connect();
    expect(info.presets.map((preset) => preset.name)).toContain("bass_boost");

    const player = manager.player("2");
    await player.setPreset("bass_boost", { gain_db: 9 });
    const graph = player.graph!;
    expect(graph.nodes.find((node) => node.id === "bass")?.params["makeup"]).toBe(9);

    // the preset's graph is a starting point
    await player.setGraph(append(withParams(graph, "bass", { makeup: 12 }), { type: "reverb", id: "room" }));
    expect(player.graph?.nodes.find((node) => node.id === "bass")?.params["makeup"]).toBe(12);
    expect(player.graph?.nodes.at(-1)?.id).toBe("room");

    expect((await manager.buildPreset("nightcore")).rate).toBe(1.25);
    expect((await failure(player.setPreset("warp"))).code).toBe("bad_request");
    await player.destroy();
  });

  test("finds artwork on the web and on the engine", async () => {
    const manager = client();
    await manager.connect();
    const covered = await loadTrack(manager, engine.covered);
    const location = manager.artwork(covered);
    expect(location?.public).toBe(false);
    expect(location?.url).toStartWith(engine.url.replace("ws:", "http:") + "/v1/artwork/");
    const image = await manager.fetchArtwork(covered);
    expect(image?.type).toBe("image/jpeg");
    expect(image?.size).toBeGreaterThan(100);

    const plain = await loadTrack(manager, engine.tone);
    expect(manager.artwork(plain)).toBeNull();
    expect(await manager.fetchArtwork(plain)).toBeNull();
    const elsewhere = { ...plain, info: { ...plain.info, artworkUrl: "https://i.ytimg.com/vi/x/hqdefault.jpg" } };
    expect(manager.artwork(elsewhere)).toEqual({ url: "https://i.ytimg.com/vi/x/hqdefault.jpg", public: true });
  });

  test("load and search errors carry the engine's error code", async () => {
    const manager = client();
    await manager.connect();
    // the engine refuses to fetch from its own network
    const error = await failure(manager.load("http://127.0.0.1:9/track.mp3"));
    expect(error.code).toBe("bad_request");
    expect((await failure(manager.search("tone", { prefix: "nosearch" }))).code).toBe(
      "bad_request",
    );
  });

  test("mirrors the queue, and fetches it again after missing changes", async () => {
    const manager = client();
    await manager.connect();
    const track = await loadTrack(manager, engine.tone);
    const player = manager.player("5");
    const { queue } = player;

    // with nothing playing, the first item starts, having left the queue already
    const started = nextCall<[Track, string]>();
    player.once("trackStart", started.listener);
    const queueAtStart = nextCall<[string[]]>();
    player.once("trackStart", () => queueAtStart.listener(queue.items.map((item) => item.id)));
    const added = await queue.add([track, track, engine.tone], { userData: { n: 1 } });
    const [first, second, third] = added.items.map((item) => item.id);
    if (first === undefined || second === undefined || third === undefined) {
      throw new Error("three items were added");
    }
    expect(added.items[2]?.track.userData).toEqual({ n: 1 });
    expect((await started.called)[1]).toBe(first);
    expect(player.itemId).toBe(first);
    expect((await queueAtStart.called)[0]).toEqual([second, third]);

    await queue.move(third, 0);
    await queue.setRepeat("queue");
    expect(queue.items.map((item) => item.id)).toEqual([third, second]);
    expect(queue.repeat).toBe("queue");
    expect(queue.version).toBe(4);

    // a crossfade is no change to the queue's items, so no version counts it
    await queue.setCrossfade(3000);
    expect(queue.crossfade).toBe(3000);
    expect((await failure(queue.setCrossfade(20_000))).code).toBe("bad_request");
    await player.refresh();
    expect(queue.crossfade).toBe(3000);
    expect(queue.version).toBe(4);

    // a change that skips a version shows that some were missed: the queue is fetched again
    const synced = nextCall<[]>();
    queue.once("sync", synced.listener);
    player._handleEvent({
      event: "queueChanged",
      guildId: "5",
      version: queue.version + 2,
      change: { type: "cleared" },
    });
    await synced.called;
    expect(queue.items.map((item) => item.id)).toEqual([third, second]);
    expect(queue.version).toBe(4);

    // the next item is loaded already, so skipping is instant; repeating the queue, the
    // skipped item goes to its end
    const skipped = await player.skip();
    expect(skipped.itemId).toBe(third);
    expect(queue.items.map((item) => item.id)).toEqual([second, first]);

    await queue.setRepeat("off");
    await queue.clear();
    expect(queue.items).toEqual([]);
    const ended = nextCall<[]>();
    player.once("queueEnd", ended.listener);
    await player.skip();
    await ended.called;
    expect(player.track).toBeNull();
    await player.destroy();
  });

  test("plays clips on layers of their own, mirrored from responses and events", async () => {
    const manager = client();
    await manager.connect();
    const player = manager.player("11");
    const voice = player.layer("voice");
    await voice.configure({ volume: 80, duck: { layers: ["main"], volume: 25 } });
    expect(voice.volume).toBe(80);
    expect(voice.duck).toEqual({ layers: ["main"], volume: 25, attack: 50, release: 500, hold: 250 });

    // a clip that starts right away says so before `play` resolves
    const order: string[] = [];
    voice.once("clipStart", (_, clipId) => order.push(`start ${clipId}`));
    const heard = nextCall<[string, Track, string]>();
    player.once("clipStart", heard.listener);
    const first = await voice.play(engine.tone, { clipId: "hello" });
    order.push("played");
    expect(order).toEqual(["start hello", "played"]);
    expect((await heard.called)[0]).toBe("voice");
    expect(first.status).toBe("playing");
    const second = await voice.play(engine.tone);
    expect(second.status).toBe("waiting");
    expect(voice.clips.map((clip) => [clip.clipId, clip.status])).toEqual([
      ["hello", "playing"],
      [second.clipId, "waiting"],
    ]);
    expect(player.track).toBeNull();

    // a refused change changes nothing, and refreshing restores the layer from the engine
    expect((await failure(voice.configure({ mode: "mix" }))).code).toBe("bad_request");
    expect(voice.mode).toBe("queue");
    voice.volume = 1;
    await player.refresh();
    expect(voice.volume).toBe(80);
    expect(voice.clips).toHaveLength(2);

    const ended: string[] = [];
    voice.on("clipEnd", (_, reason, clipId) => ended.push(`${clipId} ${reason}`));
    await voice.stop();
    expect(voice.clips).toEqual([]);
    expect(ended.sort()).toEqual([`${second.clipId} stopped`, "hello stopped"].sort());
    await voice.remove();
    expect((await player.update({})).layers).toEqual([]);
    expect((await failure(player.layer("main").play(engine.tone))).code).toBe("bad_request");
    await player.destroy();
  });

  test("uploads files to play, within the engine's limits", async () => {
    const manager = client();
    const info = await manager.connect();
    expect(info.uploads).toEqual({ maxBytes: 1 << 20, keep: 3_600_000 });

    const upload = await manager.upload(Bun.file(engine.tone), { title: "Doorbell" });
    expect(upload.track.info).toMatchObject({ title: "Doorbell", sourceName: "upload" });
    // with the last Opus packet's padding
    expect(upload.track.info.length! - 30_000).toBeWithin(0, 25);
    expect(upload.track.info.uri).toBe(`upload:${upload.id}`);
    const clip = await manager.player("6").layer("sfx").play(upload.track);
    expect(clip.status).toBe("playing");

    // refused before anything is sent
    expect((await failure(manager.upload(new Uint8Array((1 << 20) + 1)))).code).toBe("too_large");
    const unplayable = await failure(manager.upload(new TextEncoder().encode("only words")));
    expect(unplayable.code).toBe("no_audio");

    await manager.deleteUpload(upload.id);
    expect((await failure(manager.load(`upload:${upload.id}`))).code).toBe("unavailable");
    expect((await failure(manager.deleteUpload(upload.id))).code).toBe("not_found");
    await manager.player("6").destroy();
  });

  test("assembles voice details from gateway events and passes them on", async () => {
    const sent: Array<[string, GatewayVoiceStateUpdate]> = [];
    const manager = client({ sendToGateway: (guildId, payload) => sent.push([guildId, payload]) });
    await manager.connect();
    const player = manager.player("7");

    player.connect("70");
    expect(sent).toEqual([
      ["7", { op: 4, d: { guild_id: "7", channel_id: "70", self_mute: false, self_deaf: true } }],
    ]);

    // Discord answers with both updates; nothing listens on this endpoint, so connecting fails
    const closed = nextCall<[{ reason: string }]>(20_000);
    player.on("voiceClosed", closed.listener);
    manager.handleRawEvent({
      t: "VOICE_STATE_UPDATE",
      d: { guild_id: "7", user_id: BOT, session_id: "voice-session", channel_id: "70" },
    });
    manager.handleRawEvent({ t: "VOICE_SERVER_UPDATE", d: { guild_id: "7", token: "t", endpoint: null } });
    manager.handleRawEvent({
      t: "VOICE_SERVER_UPDATE",
      d: { guild_id: "7", token: "t", endpoint: "127.0.0.1:9" },
    });
    await closed.called;
    expect(player.channelId).toBe("70");

    // the bot leaving the channel destroys the player
    const destroyed = nextCall<[]>();
    player.on("destroyed", destroyed.listener);
    manager.handleRawEvent({
      t: "VOICE_STATE_UPDATE",
      d: { guild_id: "7", user_id: BOT, session_id: "voice-session", channel_id: null },
    });
    await destroyed.called;
    expect(sent.at(-1)?.[1].d.channel_id).toBeNull();
  }, 30_000);

  test("ignores voice states of the bot's other gateway sessions, reporting the conflict", async () => {
    const sent: Array<[string, GatewayVoiceStateUpdate]> = [];
    const manager = client({ sendToGateway: (guildId, payload) => sent.push([guildId, payload]) });
    await manager.connect();
    const errors: Array<[string, string | undefined]> = [];
    manager.on("error", (error, guildId) => errors.push([error.code, guildId]));
    manager.handleRawEvent({ t: "READY", d: { session_id: "mine" } });
    const player = manager.player("8");
    player.connect("80");
    const destroyed = nextCall<[]>(500);
    player.on("destroyed", destroyed.listener);

    // e.g. a second instance of the bot joining; reported once, not passed on
    for (const channel_id of ["80", "80", null]) {
      manager.handleRawEvent({
        t: "VOICE_STATE_UPDATE",
        d: { guild_id: "8", user_id: BOT, session_id: "theirs", channel_id },
      });
    }
    expect(errors).toEqual([["voice_conflict", "8"]]);
    expect(player.channelId).toBeNull();
    await expect(destroyed.called).rejects.toThrow();

    manager.handleRawEvent({
      t: "VOICE_STATE_UPDATE",
      d: { guild_id: "8", user_id: BOT, session_id: "mine", channel_id: "80" },
    });
    expect(player.channelId).toBe("80");
    await player.destroy();
  });

  test("keeps a text channel for the player's lifetime", async () => {
    const manager = client();
    await manager.connect();
    const player = manager.player("9");
    expect(player.textChannelId).toBeNull();
    const changes: Array<[string | null, string | null]> = [];
    player.on("textChannelChange", (channelId, previous) => changes.push([channelId, previous]));

    player.setTextChannel("90");
    player.setTextChannel("90");
    player.setTextChannel("91");
    player.setTextChannel(null);
    player.setTextChannel("92");
    expect(changes).toEqual([
      ["90", null],
      ["91", "90"],
      [null, "91"],
      ["92", null],
    ]);

    // still readable to post about its end, but it cannot move any more
    await player.destroy();
    expect(player.textChannelId).toBe("92");
    expect(() => player.setTextChannel("93")).toThrow(EntroximerError);
    expect(manager.player("9").textChannelId).toBeNull();
    await manager.player("9").destroy();
  });

  test("resumes its session after the connection drops", async () => {
    const manager = client();
    await manager.connect();
    const player = manager.player("3");
    await player.play(engine.tone);

    const resumed = nextCall<[{ resumed: boolean }]>();
    manager.on("ready", resumed.listener);
    const updated = nextCall<[unknown]>();
    player.on("update", updated.listener);
    manager.connection.reconnect();

    expect((await resumed.called)[0].resumed).toBe(true);
    await updated.called;
    expect(manager.players.get("3")).toBe(player);
    expect(player.track?.info.title).toBe("tone");
    // requests work again on the new connection
    expect((await player.setVolume(80)).volume).toBe(80);
  });
});
