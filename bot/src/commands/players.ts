// The guild's player, for the commands that act on it.
import type { Player } from "entroximer-client";

import type { Engine } from "../engine";
import { UserError } from "./command";
import type { GuildInteraction } from "./command";

export function existingPlayer(interaction: GuildInteraction, engine: Engine): Player {
  const player = engine.manager.players.get(interaction.guildId);
  if (!player) {
    throw new UserError("Nothing is playing here.");
  }
  return player;
}

/** The guild's player, joining the user's voice channel if it is not in one yet. */
export function joinedPlayer(interaction: GuildInteraction, engine: Engine): Player {
  const player = engine.manager.player(interaction.guildId);
  if (player.channelId === null) {
    const channelId = voiceChannelOf(interaction);
    engine.messages.bindNew(player, interaction.channel, channelId);
    player.connect(channelId);
  }
  return player;
}

export function voiceChannelOf(interaction: GuildInteraction): string {
  const channelId = interaction.member.voice.channelId;
  if (!channelId) {
    throw new UserError("Join a voice channel first, or name one.");
  }
  return channelId;
}

/** The `userData` of what a command plays or queues: who asked for it. */
export function requester(interaction: GuildInteraction): { requester: string } {
  return { requester: interaction.user.id };
}
