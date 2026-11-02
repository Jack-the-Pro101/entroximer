import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Entroximer, EntroximerError, PROTOCOL_VERSION, append, withParams } from "../src";
import type {
  EntroximerOptions,
  GatewayVoiceStateUpdate,
  Track,
  TrackEndReason,
} from "../src";
import { canRunEngine, nextCall, startEngine } from "./engine";
import type { TestEngine } from "./engine";

const BOT = "1000";
const PASSWORD = "test password";

describe.skipIf(!canRunEngine)("against a real engine", () => {
  let engine: TestEngine;
  const clients: Entroximer[] = [];

  beforeAll(async () => {
    engine = await startEngine({ password: PASSWORD });
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
    expect(graph.nodes.find((node) => node.id === "bass")?.params["gain_db"]).toBe(9);

    // the preset's graph is a starting point
    await player.setGraph(append(withParams(graph, "bass", { gain_db: 12 }), { type: "reverb", id: "room" }));
    expect(player.graph?.nodes.find((node) => node.id === "bass")?.params["gain_db"]).toBe(12);
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
