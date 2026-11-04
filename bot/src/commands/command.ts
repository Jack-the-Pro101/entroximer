// What a slash command is, and what the commands share.
import { InteractionContextType, SlashCommandBuilder } from "discord.js";
import type { AutocompleteInteraction, ChatInputCommandInteraction, SharedSlashCommand } from "discord.js";
import { EntroximerError } from "entroximer-client";

import type { Engine } from "../engine";

export type GuildInteraction = ChatInputCommandInteraction<"cached">;
export type GuildAutocomplete = AutocompleteInteraction<"cached">;

export interface Command {
  data: SharedSlashCommand;
  execute(interaction: GuildInteraction, engine: Engine): Promise<void>;
  /** Suggests values for the option being typed. */
  autocomplete?(interaction: GuildAutocomplete, engine: Engine): Promise<void>;
}

/** A failure to explain to the user. */
export class UserError extends Error {}

/** What to tell the user about a failure; failures that are no fault of theirs are also logged. */
export function failureText(error: unknown, what: string): string {
  if (error instanceof UserError || error instanceof EntroximerError) {
    if (!(error instanceof UserError)) {
      console.error(`${what} failed:`, error);
    }
    return error.message;
  }
  console.error(`${what} failed:`, error);
  return "Something went wrong; see the bot's log.";
}

/** A command used in guilds only. */
export function command(
  build: (builder: SlashCommandBuilder) => SharedSlashCommand,
  execute: Command["execute"],
  autocomplete?: Command["autocomplete"],
): Command {
  const data = build(new SlashCommandBuilder().setContexts(InteractionContextType.Guild));
  return autocomplete ? { data, execute, autocomplete } : { data, execute };
}

/** Discord waits 3 s for suggestions. */
export const SUGGESTION_TIMEOUT_MS = 2_500;

export function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`no answer within ${ms} ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}
