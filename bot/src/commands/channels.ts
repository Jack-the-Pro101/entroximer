// Where the bot plays and posts: /join, /channel, /leave.
import { ChannelType } from "discord.js";
import type { Player } from "entroximer-client";

import { whyCannotPostIn } from "../messages";
import { UserError, command } from "./command";
import type { Command, GuildInteraction } from "./command";
import { existingPlayer, voiceChannelOf } from "./players";

/** Channels that hold messages; voice channels have a chat too. */
const POSTABLE_CHANNEL_TYPES = [
  ChannelType.GuildText,
  ChannelType.GuildAnnouncement,
  ChannelType.GuildVoice,
  ChannelType.GuildStageVoice,
  ChannelType.PublicThread,
  ChannelType.PrivateThread,
  ChannelType.AnnouncementThread,
] as const;

export const channelCommands: Command[] = [
  command(
    (b) =>
      b
        .setName("join")
        .setDescription("Join your voice channel, or the one given")
        .addChannelOption((o) =>
          o
            .setName("channel")
            .setDescription("The voice channel to join")
            .addChannelTypes(ChannelType.GuildVoice, ChannelType.GuildStageVoice),
        ),
    async (interaction, engine) => {
      const channelId = interaction.options.getChannel("channel")?.id ?? voiceChannelOf(interaction);
      const player = engine.manager.player(interaction.guildId);
      engine.messages.bindNew(player, interaction.channel, channelId);
      player.connect(channelId);
      await interaction.reply(`Joining <#${channelId}>.${postingNote(player, interaction)}`);
    },
  ),
  command(
    (b) =>
      b
        .setName("channel")
        .setDescription("Choose where the bot posts about playback, or see where it does")
        .addChannelOption((o) =>
          o
            .setName("channel")
            .setDescription("Post about playback here, until the bot leaves")
            .addChannelTypes(...POSTABLE_CHANNEL_TYPES),
        )
        .addBooleanOption((o) =>
          o.setName("silent").setDescription("Only post problems, not what starts playing, until the bot leaves"),
        ),
    async (interaction, engine) => {
      const player = existingPlayer(interaction, engine);
      const channel = interaction.options.getChannel("channel");
      const silent = interaction.options.getBoolean("silent");
      const said: string[] = [];
      if (channel) {
        const why = whyCannotPostIn(channel);
        if (why !== null) {
          throw new UserError(`I can't post in <#${channel.id}>: ${why}.`);
        }
        player.setTextChannel(channel.id);
        said.push(`Playback messages go to <#${channel.id}> now.`);
      }
      if (silent !== null) {
        engine.messages.setSilent(player, silent);
        said.push(silent ? "Silent: only problems are posted until the bot leaves." : "No longer silent.");
      }
      if (said.length === 0) {
        const where = player.textChannelId;
        said.push(
          where === null
            ? "Not posting about playback anywhere; `/channel` with a channel chooses where."
            : `Posting about playback in <#${where}>.`,
          ...(engine.messages.isSilent(player) ? ["Silent: only problems, not what starts playing."] : []),
        );
      }
      await interaction.reply(said.join(" "));
    },
  ),
  command(
    (b) => b.setName("leave").setDescription("Stop playback and leave the voice channel"),
    async (interaction, engine) => {
      await existingPlayer(interaction, engine).destroy();
      await interaction.reply("Left the voice channel.");
    },
  ),
];

/** For a reply in `interaction`'s channel: where playback messages go, if not there. */
function postingNote(player: Player, interaction: GuildInteraction): string {
  if (player.textChannelId === null) {
    return " I can't post about playback here or in the voice channel's chat; `/channel` chooses where.";
  }
  return player.textChannelId === interaction.channelId ? "" : ` Playback messages go to <#${player.textChannelId}>.`;
}
