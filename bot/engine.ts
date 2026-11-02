// The bot's connection to the engine, and the messages it posts about playback.
import { Events } from "discord.js";
import type { Client } from "discord.js";
import { Entroximer } from "entroximer-client";
import type { Event, GatewayDispatch } from "entroximer-client";

import { Announcements } from "./announcements";
import type { PostKind } from "./announcements";
import { env } from "./env";
import { describeTrack, formatTime } from "./format";

export interface Engine {
  manager: Entroximer;
  /** Posts about playback in each player's text channel. */
  announcements: Announcements;
}

/**
 * Keeps the gateway's `READY` until the engine's client exists, which needs the bot's user ID and
 * so only starts at `clientReady`; `READY` tells it which voice states are this process's. Call
 * before logging in.
 */
export function keepGatewayReady(client: Client): GatewayDispatch[] {
  const kept: GatewayDispatch[] = [];
  const keep = (packet: GatewayDispatch) => {
    if (packet.t === "READY") {
      kept.push(packet);
    }
  };
  client.on(Events.Raw, keep);
  client.once(Events.ClientReady, () => client.off(Events.Raw, keep));
  return kept;
}

export async function connectEngine(client: Client<true>, gatewayReady: GatewayDispatch[] = []): Promise<Engine> {
  const manager = new Entroximer({
    url: env.ENTROXIMER_URL,
    password: env.ENTROXIMER_PASSWORD,
    userId: client.user.id,
    clientName: "entroximer-dev-bot/0.1.0",
    sendToGateway: (guildId, payload) => client.guilds.cache.get(guildId)?.shard.send(payload),
  });
  for (const packet of gatewayReady) {
    manager.handleRawEvent(packet);
  }
  client.on(Events.Raw, (packet) => manager.handleRawEvent(packet));

  const announcements = new Announcements(client, manager);
  manager.on("event", (event) => {
    const post = describeEvent(event);
    const player = "guildId" in event ? manager.players.get(event.guildId) : undefined;
    if (post && player) {
      announcements.post(player, post.content, post.kind);
    }
  });
  manager.on("youtubeSession", (state) => {
    if (state === "expired") {
      notifyOwner(
        client,
        "The engine's YouTube session expired, so age-restricted videos cannot play. Export " +
          "fresh cookies (see the engine README) and run `entroximer youtube cookies import`.",
      );
    }
  });
  manager.on("disconnect", ({ code, reason, willReconnect }) =>
    console.warn(`engine connection lost (${code} ${reason})${willReconnect ? "; reconnecting" : ""}`),
  );
  manager.on("ready", ({ resumed }) => console.log(`engine connected${resumed ? " (session resumed)" : ""}`));
  manager.on("playersLost", (players) => {
    for (const player of players) {
      announcements.post(player, "Lost the engine's player (it restarted, or reconnecting took too long); playback stopped.", "problem");
    }
  });
  manager.on("error", (error, guildId) => {
    console.error(`engine error${guildId ? ` in guild ${guildId}` : ""}:`, error);
    const player = guildId === undefined ? undefined : manager.players.get(guildId);
    if (error.code === "voice_conflict" && player) {
      announcements.post(
        player,
        "⚠️ Another instance of this bot joined voice here, which takes over its voice connection. Stop the other one.",
        "problem",
      );
    }
  });

  const info = await manager.connect();
  console.log(`engine ${info.version} (yt-dlp ${info.ytdlp ?? "unknown"}, YouTube session ${info.youtubeSession})`);
  return { manager, announcements };
}

function describeEvent(event: Event): { content: string; kind: PostKind } | null {
  switch (event.event) {
    case "trackStart":
      return { content: `▶️ Now playing ${describeTrack(event.track)}`, kind: "routine" };
    case "trackError":
      return { content: `⚠️ ${describeTrack(event.track)} failed: ${event.error.message}`, kind: "problem" };
    case "trackStuck":
      return {
        content: `⏳ Still waiting for ${describeTrack(event.track)} after ${formatTime(event.waiting)}.`,
        kind: "problem",
      };
    case "queueEnd":
      return { content: "⏹️ The queue has ended.", kind: "routine" };
    case "clipError":
      return { content: `⚠️ The sound ${describeTrack(event.track)} failed: ${event.error.message}`, kind: "problem" };
    case "clipStuck":
      return {
        content: `⏳ Gave up on the sound ${describeTrack(event.track)} after ${formatTime(event.waiting)} without audio.`,
        kind: "problem",
      };
    case "voiceClosed":
      // 4006: the voice session was replaced, e.g. by the bot joining from another session
      return event.byRemote
        ? { content: `🔌 Discord closed the voice connection (${event.code ?? "no code"}).`, kind: "problem" }
        : null;
    default:
      return null;
  }
}

async function notifyOwner(client: Client<true>, content: string): Promise<void> {
  try {
    const application = await client.application.fetch();
    const owner =
      application.owner && "members" in application.owner
        ? application.owner.owner?.user
        : application.owner;
    await owner?.send(content);
  } catch (error) {
    console.error("cannot notify the bot's owner:", error);
  }
}
