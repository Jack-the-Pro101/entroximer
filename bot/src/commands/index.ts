// The slash commands, and running them.
import { DiscordAPIError, MessageFlags, RESTJSONErrorCodes } from "discord.js";

import type { Engine } from "../engine";
import { channelCommands } from "./channels";
import { failureText } from "./command";
import type { Command, GuildAutocomplete, GuildInteraction } from "./command";
import { graphCommands } from "./graph";
import { infoCommands } from "./info";
import { playCommands } from "./play";
import { playbackCommands } from "./playback";
import { queueCommands } from "./queue";
import { soundCommands } from "./sound";

export const commands: readonly Command[] = [
  ...channelCommands,
  ...playCommands,
  ...queueCommands,
  ...playbackCommands,
  ...soundCommands,
  ...graphCommands,
  ...infoCommands,
];

const byName = new Map(commands.map((command) => [command.data.name, command]));

/** Runs a command, answering failures with an explanation only its user sees. */
export async function run(interaction: GuildInteraction, engine: Engine): Promise<void> {
  const found = byName.get(interaction.commandName);
  if (!found) {
    return;
  }
  try {
    await found.execute(interaction, engine);
  } catch (error) {
    if (answeredElsewhere(error)) {
      warnAnsweredElsewhere();
      return;
    }
    const message = failureText(error, `/${interaction.commandName}`);
    const reply = { content: `❌ ${message}`, embeds: [], components: [], allowedMentions: { parse: [] } };
    if (interaction.deferred || interaction.replied) {
      await interaction.editReply({ ...reply, attachments: [] });
    } else {
      await interaction.reply({ ...reply, flags: MessageFlags.Ephemeral });
    }
  }
}

/** Answers an autocomplete request; without suggestions if they take too long. */
export async function suggest(interaction: GuildAutocomplete, engine: Engine): Promise<void> {
  await byName.get(interaction.commandName)?.autocomplete?.(interaction, engine);
}

/**
 * Whether Discord refused an answer because the interaction was answered already: Discord sends
 * each interaction to every session of the bot, so another instance of it answered first.
 */
export function answeredElsewhere(error: unknown): boolean {
  return error instanceof DiscordAPIError && error.code === RESTJSONErrorCodes.InteractionHasAlreadyBeenAcknowledged;
}

let warnedAnsweredElsewhere = false;

export function warnAnsweredElsewhere(): void {
  if (!warnedAnsweredElsewhere) {
    warnedAnsweredElsewhere = true;
    console.warn(
      "another instance of this bot answered an interaction first: the bot is running twice with this token " +
        "(e.g. in a terminal that outlived its editor window). Stop the other one; both join voice and take " +
        "each other's voice connections.",
    );
  }
}
