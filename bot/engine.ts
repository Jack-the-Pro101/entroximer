// The bot's connection to the engine, and the messages it posts about playback.
import { Events } from "discord.js";
import type { Client, SendableChannels } from "discord.js";
import { Entroximer } from "entroximer-client";
import type { Event, GatewayDispatch } from "entroximer-client";

import { env } from "./env";
import { describeTrack, formatTime } from "./format";

export interface Engine {
  manager: Entroximer;
  /** Where to post about a guild's playback: the channel of its latest command. */
  announceIn(guildId: string, channel: SendableChannels | null): void;
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

  const channels = new Map<string, SendableChannels>();
  const announce = (guildId: string, content: string) => {
    channels
      .get(guildId)
      ?.send({ content, allowedMentions: { parse: [] } })
      .catch((error: unknown) => console.error(`cannot post in guild ${guildId}:`, error));
  };

  manager.on("event", (event) => {
    const message = describeEvent(event);
    if (message && "guildId" in event) {
      announce(event.guildId, message);
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
      announce(player.guildId, "Lost the engine's player (it restarted, or reconnecting took too long); playback stopped.");
    }
  });
  manager.on("error", (error, guildId) => {
    console.error(`engine error${guildId ? ` in guild ${guildId}` : ""}:`, error);
    if (error.code === "voice_conflict" && guildId) {
      announce(guildId, "⚠️ Another instance of this bot joined voice here, which takes over its voice connection. Stop the other one.");
    }
  });

  const info = await manager.connect();
  console.log(`engine ${info.version} (yt-dlp ${info.ytdlp ?? "unknown"}, YouTube session ${info.youtubeSession})`);
  return {
    manager,
    announceIn(guildId, channel) {
      if (channel) {
        channels.set(guildId, channel);
      }
    },
  };
}

function describeEvent(event: Event): string | null {
  switch (event.event) {
    case "trackStart":
      return `▶️ Now playing ${describeTrack(event.track)}`;
    case "trackError":
      return `⚠️ ${describeTrack(event.track)} failed: ${event.error.message}`;
    case "trackStuck":
      return `⏳ Still waiting for ${describeTrack(event.track)} after ${formatTime(event.waiting)}.`;
    case "queueEnd":
      return "⏹️ The queue has ended.";
    case "voiceClosed":
      // 4006: the voice session was replaced, e.g. by the bot joining from another session
      return event.byRemote ? `🔌 Discord closed the voice connection (${event.code ?? "no code"}).` : null;
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
