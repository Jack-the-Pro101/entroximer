// The queue: /queue, /skip, /remove, /move, /trim, /shuffle, /clear, /repeat, /crossfade.
import type { Player, QueueItem, Repeat } from "entroximer-client";

import { queueMessage } from "../embeds";
import { TIME_RANGE_HELP, describeItem, describeTrack, parseTimeRange } from "../format";
import { UserError, command } from "./command";
import type { Command } from "./command";
import { existingPlayer } from "./players";

export const queueCommands: Command[] = [
  command(
    (b) =>
      b
        .setName("queue")
        .setDescription("Show the queue")
        .addIntegerOption((o) => o.setName("page").setDescription("Which page").setMinValue(1)),
    async (interaction, engine) => {
      const player = existingPlayer(interaction, engine);
      // artwork on the engine is uploaded with the reply, which may take a moment
      await interaction.deferReply();
      await interaction.editReply(await queueMessage(engine.manager, player, interaction.options.getInteger("page") ?? 1));
    },
  ),
  command(
    (b) =>
      b
        .setName("skip")
        .setDescription("Skip to the next track in the queue, or to the one given")
        .addIntegerOption((o) =>
          o.setName("to").setDescription("The position in the queue to skip to").setMinValue(1),
        ),
    async (interaction, engine) => {
      const player = existingPlayer(interaction, engine);
      if (!player.track && player.queue.length === 0) {
        throw new UserError("Nothing to skip.");
      }
      const to = interaction.options.getInteger("to");
      const target = to === null ? undefined : itemAt(player, to);
      const skipped = player.track;
      const next = target ?? player.queue.items[0];
      await player.skip(target?.id);
      const what = skipped ? `Skipped ${describeTrack(skipped)}` : "Skipped";
      await interaction.reply(next ? `${what}; up now: ${describeItem(next)}` : `${what}. The queue is empty.`);
    },
  ),
  command(
    (b) =>
      b
        .setName("remove")
        .setDescription("Remove a track from the queue")
        .addIntegerOption((o) =>
          o.setName("position").setDescription("Its position in the queue").setMinValue(1).setRequired(true),
        ),
    async (interaction, engine) => {
      const player = existingPlayer(interaction, engine);
      const item = itemAt(player, interaction.options.getInteger("position", true));
      await player.queue.remove(item.id);
      await interaction.reply(`Removed ${describeItem(item)}.`);
    },
  ),
  command(
    (b) =>
      b
        .setName("move")
        .setDescription("Move a track within the queue")
        .addIntegerOption((o) =>
          o.setName("from").setDescription("Its position now").setMinValue(1).setRequired(true),
        )
        .addIntegerOption((o) =>
          o.setName("to").setDescription("Its new position").setMinValue(1).setRequired(true),
        ),
    async (interaction, engine) => {
      const player = existingPlayer(interaction, engine);
      const item = itemAt(player, interaction.options.getInteger("from", true));
      const to = Math.min(interaction.options.getInteger("to", true), player.queue.length);
      await player.queue.move(item.id, to - 1);
      await interaction.reply(`Moved ${describeItem(item)} to position ${to}.`);
    },
  ),
  command(
    (b) =>
      b
        .setName("trim")
        .setDescription("Change the part of the current track, or of a queued one, that plays")
        .addStringOption((o) =>
          o
            .setName("time")
            .setDescription("1:30 (from), 1:30-2:45 (from-to) or -2:45 (to); 0 is the beginning, end the end")
            .setMaxLength(40)
            .setRequired(true),
        )
        .addIntegerOption((o) =>
          o.setName("position").setDescription("Its position in the queue; the current track if absent").setMinValue(1),
        ),
    async (interaction, engine) => {
      const player = existingPlayer(interaction, engine);
      const range = parseTimeRange(interaction.options.getString("time", true));
      if (range === null) {
        throw new UserError(TIME_RANGE_HELP);
      }
      const position = interaction.options.getInteger("position");
      const queued = position === null ? null : itemAt(player, position);
      const id = queued?.id ?? player.itemId;
      if (id === null) {
        throw new UserError("Nothing is playing; give the position of a queued track.");
      }
      const info = await player.setTimes(id, range);
      if (queued !== null) {
        // its new times came with the queue's change
        const item = player.queue.items.find((candidate) => candidate.id === id) ?? queued;
        await interaction.reply(`Position ${position} now plays ${describeItem(item)}.`);
        return;
      }
      // it plays on from where it is
      const later = range.startTime !== undefined ? " Its new start applies the next time it plays." : "";
      await interaction.reply(`Now playing ${info.track ? describeTrack(info.track) : "nothing"}.${later}`);
    },
  ),
  command(
    (b) => b.setName("shuffle").setDescription("Shuffle the queue"),
    async (interaction, engine) => {
      const { length } = await existingPlayer(interaction, engine).queue.shuffle();
      await interaction.reply(`Shuffled ${length} tracks.`);
    },
  ),
  command(
    (b) => b.setName("clear").setDescription("Remove every track from the queue"),
    async (interaction, engine) => {
      await existingPlayer(interaction, engine).queue.clear();
      await interaction.reply("Cleared the queue.");
    },
  ),
  command(
    (b) =>
      b
        .setName("repeat")
        .setDescription("Repeat the current track or the queue")
        .addStringOption((o) =>
          o
            .setName("mode")
            .setDescription("What to repeat")
            .setRequired(true)
            .addChoices(
              { name: "Nothing", value: "off" },
              { name: "The current track", value: "track" },
              { name: "The queue: finished tracks go to its end", value: "queue" },
            ),
        ),
    async (interaction, engine) => {
      const mode = interaction.options.getString("mode", true);
      if (!isRepeat(mode)) {
        throw new UserError(`Unknown mode \`${mode}\`.`);
      }
      await existingPlayer(interaction, engine).queue.setRepeat(mode);
      const said: Record<Repeat, string> = {
        off: "Not repeating.",
        track: "Repeating the current track.",
        queue: "Repeating the queue.",
      };
      await interaction.reply(said[mode]);
    },
  ),
  command(
    (b) =>
      b
        .setName("crossfade")
        .setDescription("Fade each track into the next")
        .addNumberOption((o) =>
          o
            .setName("seconds")
            .setDescription("How long tracks overlap; 0 plays them back to back")
            .setRequired(true)
            .setMinValue(0)
            .setMaxValue(12),
        ),
    async (interaction, engine) => {
      const seconds = interaction.options.getNumber("seconds", true);
      await existingPlayer(interaction, engine).queue.setCrossfade(seconds * 1000);
      await interaction.reply(
        seconds === 0
          ? "Tracks play back to back."
          : `Tracks crossfade over ${seconds} s (at most half of a track).`,
      );
    },
  ),
];

function isRepeat(text: string): text is Repeat {
  return text === "off" || text === "track" || text === "queue";
}

/** The queue item at `position`, counted from 1. */
function itemAt(player: Player, position: number): QueueItem {
  const item = player.queue.items[position - 1];
  if (!item) {
    throw new UserError(`The queue has ${player.queue.length} tracks.`);
  }
  return item;
}
