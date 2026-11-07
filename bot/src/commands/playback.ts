// The current track: /pause, /resume, /stop, /seek, /nowplaying.
import { nowPlayingMessage } from "../embeds";
import { formatTime, parseTime } from "../format";
import { UserError, command } from "./command";
import type { Command } from "./command";
import { existingPlayer } from "./players";

export const playbackCommands: Command[] = [
  command(
    (b) => b.setName("pause").setDescription("Pause playback"),
    async (interaction, engine) => {
      await existingPlayer(interaction, engine).pause();
      await interaction.reply("Paused.");
    },
  ),
  command(
    (b) => b.setName("resume").setDescription("Resume playback"),
    async (interaction, engine) => {
      await existingPlayer(interaction, engine).resume();
      await interaction.reply("Resumed.");
    },
  ),
  command(
    (b) => b.setName("stop").setDescription("Stop playback; the queue waits until something plays"),
    async (interaction, engine) => {
      await existingPlayer(interaction, engine).stop();
      await interaction.reply("Stopped. `/skip` continues with the queue.");
    },
  ),
  command(
    (b) =>
      b
        .setName("seek")
        .setDescription("Jump to a position in the current track")
        .addStringOption((o) =>
          o.setName("position").setDescription("Like 90, 1:30, 1:02:03 or 1m30s").setRequired(true),
        ),
    async (interaction, engine) => {
      const position = parseTime(interaction.options.getString("position", true));
      if (position === null) {
        throw new UserError("Give the position as seconds, `m:ss`, `h:mm:ss` or `1m30s`.");
      }
      const player = existingPlayer(interaction, engine);
      if (!player.track) {
        throw new UserError("Nothing is playing.");
      }
      await player.seek(position);
      await interaction.reply(`Jumped to ${formatTime(position)}.`);
    },
  ),
  command(
    (b) => b.setName("nowplaying").setDescription("Show what is playing"),
    async (interaction, engine) => {
      const player = existingPlayer(interaction, engine);
      if (!player.track) {
        throw new UserError("Nothing is playing.");
      }
      await interaction.deferReply();
      await interaction.editReply(await nowPlayingMessage(engine.manager, player, player.track));
    },
  ),
];
