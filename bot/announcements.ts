// Where and whether the bot posts about a guild's playback. Each player has a text channel, kept by
// the client library for as long as the player lives: the command channel that made the bot join
// voice, until `/channel` moves it. When the bot can no longer post there, it moves to the chat of
// the voice channel, or stops posting.
import { DiscordAPIError, Events, PermissionFlagsBits, RESTJSONErrorCodes } from "discord.js";
import type { Channel, Client } from "discord.js";
import type { Entroximer, Player } from "entroximer-client";

/** Silent players leave out routine posts (what starts playing, the queue ending); never problems. */
export type PostKind = "routine" | "problem";

/** Why Discord refused a post because of the channel, rather than e.g. a network failure. */
const LOST_CHANNEL: Partial<Record<number, string>> = {
  [RESTJSONErrorCodes.UnknownChannel]: "it was deleted",
  [RESTJSONErrorCodes.MissingAccess]: "I can't see it",
  [RESTJSONErrorCodes.MissingPermissions]: "I'm not allowed to send messages there",
  [RESTJSONErrorCodes.CannotSendMessagesInNonTextChannel]: "it isn't a text channel",
  [RESTJSONErrorCodes.InvalidActionOnArchivedThread]: "the thread is archived",
  [RESTJSONErrorCodes.ThreadLocked]: "the thread is locked",
};

/** Why the bot cannot post in `channel`, as far as its cache knows; `null` if it can. */
export function whyCannotPostIn(channel: Channel): string | null {
  if (channel.isDMBased() || !channel.isTextBased()) {
    return "it isn't a text channel";
  }
  const me = channel.guild.members.me;
  if (!me) {
    return "I'm not in that server";
  }
  const permissions = channel.permissionsFor(me);
  if (!permissions.has(PermissionFlagsBits.ViewChannel)) {
    return "I can't see it";
  }
  const send = channel.isThread() ? PermissionFlagsBits.SendMessagesInThreads : PermissionFlagsBits.SendMessages;
  if (!permissions.has(send)) {
    return "I'm not allowed to send messages there";
  }
  if (channel.isThread() && channel.locked && !permissions.has(PermissionFlagsBits.ManageThreads)) {
    return "the thread is locked";
  }
  return null;
}

export class Announcements {
  readonly #client: Client<true>;
  /** Players that only post problems; forgotten with the player, like its queue's repeat. */
  readonly #silent = new WeakSet<Player>();
  /** Each player's latest post: posts wait for the one before, so they arrive in order. */
  readonly #posting = new WeakMap<Player, Promise<void>>();

  constructor(client: Client<true>, manager: Entroximer) {
    this.#client = client;
    const deleted = (channel: { id: string }) => {
      for (const player of manager.players.values()) {
        if (player.textChannelId === channel.id) {
          this.#enqueue(player, () => this.#moveFrom(player, channel.id, "it was deleted").then(() => {}));
        }
      }
    };
    client.on(Events.ChannelDelete, deleted);
    client.on(Events.ThreadDelete, deleted);
  }

  isSilent(player: Player): boolean {
    return this.#silent.has(player);
  }

  setSilent(player: Player, silent: boolean): void {
    if (silent) {
      this.#silent.add(player);
    } else {
      this.#silent.delete(player);
    }
  }

  /**
   * Gives a player without a text channel one, when a command makes it join voice: the command's
   * channel, or else the chat of `voiceChannelId`; none if the bot can post in neither. A player
   * that has one keeps it, since only `/channel` moves it.
   */
  bindNew(player: Player, commandChannel: Channel | null, voiceChannelId: string): void {
    if (player.textChannelId !== null) {
      return;
    }
    const voice = this.#client.channels.cache.get(voiceChannelId) ?? null;
    const channel = [commandChannel, voice].find((c) => c !== null && whyCannotPostIn(c) === null);
    if (channel) {
      player.setTextChannel(channel.id);
    }
  }

  /** Posts in the player's text channel, if it has one; a silent player leaves out routine posts. */
  post(player: Player, content: string, kind: PostKind): void {
    if (kind === "routine" && this.#silent.has(player)) {
      return;
    }
    this.#enqueue(player, () => this.#post(player, content));
  }

  #enqueue(player: Player, task: () => Promise<void>): void {
    const done = (this.#posting.get(player) ?? Promise.resolve())
      .then(task)
      .catch((error: unknown) => console.error(`cannot post in guild ${player.guildId}:`, error));
    this.#posting.set(player, done);
  }

  async #post(player: Player, content: string): Promise<void> {
    const channelId = player.textChannelId;
    if (channelId === null) {
      return;
    }
    const why = await this.#send(channelId, content);
    // moving ends at the voice channel's chat or nowhere, so this retries at most twice
    if (why !== null && (await this.#moveFrom(player, channelId, why))) {
      await this.#post(player, content);
    }
  }

  /**
   * Sends `content`, returning `null`, or why the channel cannot be posted in. Other failures, e.g.
   * of the network, throw: they say nothing about the channel.
   */
  async #send(channelId: string, content: string): Promise<string | null> {
    try {
      const channel = this.#client.channels.cache.get(channelId) ?? (await this.#client.channels.fetch(channelId));
      if (!channel) {
        return "it was deleted";
      }
      const why = whyCannotPostIn(channel);
      if (why !== null || !channel.isSendable()) {
        return why ?? "it isn't a text channel";
      }
      await channel.send({ content, allowedMentions: { parse: [] } });
      return null;
    } catch (error) {
      const why = error instanceof DiscordAPIError && typeof error.code === "number" ? LOST_CHANNEL[error.code] : undefined;
      if (why === undefined) {
        throw error;
      }
      return why;
    }
  }

  /**
   * The bot cannot post in `from`, the player's text channel: moves its posts to the chat of its
   * voice channel, saying so there, or stops them. Returns whether the player has a channel to
   * post in, e.g. one `/channel` chose meanwhile.
   */
  async #moveFrom(player: Player, from: string, why: string): Promise<boolean> {
    if (player.textChannelId !== from) {
      return player.textChannelId !== null;
    }
    const voice = player.channelId;
    // a destroyed player has nothing left to say but its last post
    if (!player.destroyed && voice !== null && voice !== from) {
      const notice = `⚠️ I can't post in <#${from}> any more (${why}), so playback messages come here now; \`/channel\` moves them.`;
      const sent = (await this.#send(voice, notice)) === null;
      if (sent && player.textChannelId === from && !player.destroyed) {
        player.setTextChannel(voice);
        return true;
      }
    }
    if (player.textChannelId !== from) {
      return player.textChannelId !== null;
    }
    console.warn(`guild ${player.guildId}: no longer posting about playback: channel ${from}: ${why}`);
    if (!player.destroyed) {
      player.setTextChannel(null);
    }
    return false;
  }
}
