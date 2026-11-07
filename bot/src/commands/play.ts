// Finding tracks and queueing them: /play, /next, /bulk, /search.
import { ActionRowBuilder, ComponentType, ModalBuilder, StringSelectMenuBuilder, TextInputStyle } from "discord.js";
import type { Attachment, SlashCommandStringOption } from "discord.js";
import { EntroximerError, parseQueries } from "entroximer-client";
import type { PlaylistInfo, Player, QueueItem } from "entroximer-client";

import { searchMessage } from "../embeds";
import type { Engine } from "../engine";
import { TIME_RANGE_HELP, describeTrack, escape, parseTimeRange, plainTrack, trackLength, truncate } from "../format";
import { SUGGESTION_TIMEOUT_MS, UserError, command, failureText, withTimeout } from "./command";
import type { Command, GuildAutocomplete, GuildInteraction } from "./command";
import { joinedPlayer, requester, voiceChannelOf } from "./players";

/** Suggestions wait this long for more typing, so that only what is typed last is searched. */
const SUGGESTION_DELAY_MS = 350;
/** How long `/search` waits for a pick. */
const PICK_TIMEOUT_MS = 60_000;
/** How long `/bulk` waits for its form. */
const FORM_TIMEOUT_MS = 10 * 60_000;
/** The form's field for the list. */
const LIST_FIELD = "queries";
/** Lines one `/bulk` queues at most. */
const MAX_LINES = 1000;
/** The largest list file `/bulk` reads. */
const MAX_LIST_BYTES = 1024 * 1024;
/** How often `/bulk` updates its reply while its lines load. */
const PROGRESS_INTERVAL_MS = 3_000;
/** What a message holds. */
const MAX_MESSAGE = 2000;

const SEARCH_SOURCES = [
  { name: "YouTube", value: "ytsearch" },
  { name: "YouTube Music", value: "ytmsearch" },
  { name: "SoundCloud", value: "scsearch" },
  // only if the engine has a Spotify app configured; plays from YouTube
  { name: "Spotify", value: "spsearch" },
];

/**
 * Changes a command's reply; returns whether it could. Failures to (e.g. after Discord's 15
 * minutes) are only logged.
 */
type Edit = (content: string) => Promise<boolean>;

/** How one of `/bulk`'s lines loaded: the tracks it queued, or why it did not. */
type LineOutcome = { tracks: number } | { error: unknown };

export const playCommands: Command[] = [
  command(
    (b) =>
      b
        .setName("play")
        .setDescription("Play a track or playlist, or queue it if something is playing")
        .addStringOption((o) =>
          o
            .setName("query")
            .setDescription("A URL, or words to search YouTube for")
            .setRequired(true)
            .setAutocomplete(true),
        )
        .addIntegerOption((o) =>
          o.setName("start_index").setDescription("For a playlist: the track to start from").setMinValue(1),
        )
        .addStringOption(timeOption),
    async (interaction, engine) => {
      const player = joinedPlayer(interaction, engine);
      await interaction.deferReply();
      // with nothing playing, the front of the queue starts
      const idle = player.track === null;
      await queueQuery(interaction, player, idle ? 0 : undefined, idle);
    },
    suggestTracks,
  ),
  command(
    (b) =>
      b
        .setName("next")
        .setDescription("Queue a track or playlist to play right after the current one")
        .addStringOption((o) =>
          o
            .setName("query")
            .setDescription("A URL, or words to search YouTube for")
            .setRequired(true)
            .setAutocomplete(true),
        )
        .addIntegerOption((o) =>
          o.setName("start_index").setDescription("For a playlist: the track to start from").setMinValue(1),
        )
        .addStringOption(timeOption),
    async (interaction, engine) => {
      const player = joinedPlayer(interaction, engine);
      await interaction.deferReply();
      await queueQuery(interaction, player, 0, player.track === null);
    },
    suggestTracks,
  ),
  command(
    (b) =>
      b
        .setName("bulk")
        .setDescription("Queue a list of links or searches, one per line, from a form or a text file")
        .addAttachmentOption((o) =>
          o.setName("file").setDescription("A text file with one link or search per line; a form opens without one"),
        )
        .addIntegerOption((o) =>
          o
            .setName("start_index")
            .setDescription("The line to start from, not counting blank lines and # comments")
            .setMinValue(1),
        ),
    async (interaction, engine) => {
      // before asking for the list
      voiceChannelOf(interaction);
      const file = interaction.options.getAttachment("file");
      if (file) {
        await interaction.deferReply();
        const text = await readList(file);
        await queueList(interaction, engine, text, editor((content) => interaction.editReply(content)));
        return;
      }

      const formId = `bulk-${interaction.id}`;
      await interaction.showModal(
        new ModalBuilder()
          .setCustomId(formId)
          .setTitle("Queue a list")
          .addLabelComponents((label) =>
            label
              .setLabel("Links or searches, one per line")
              .setDescription("YouTube, Spotify, SoundCloud, ... Blank lines and # comments are left out.")
              .setTextInputComponent((input) =>
                input.setCustomId(LIST_FIELD).setStyle(TextInputStyle.Paragraph).setMaxLength(4000).setRequired(true),
              ),
          ),
      );
      const submitted = await interaction
        .awaitModalSubmit({
          time: FORM_TIMEOUT_MS,
          filter: (submit) => submit.customId === formId && submit.user.id === interaction.user.id,
        })
        .catch(() => null);
      if (!submitted) {
        return;
      }
      await submitted.deferReply();
      // the command's own answer was the form, so everything goes to the form's
      const edit = editor((content) => submitted.editReply(content));
      try {
        await queueList(interaction, engine, submitted.fields.getTextInputValue(LIST_FIELD), edit);
      } catch (error) {
        await edit(`❌ ${failureText(error, "/bulk")}`);
      }
    },
  ),
  command(
    (b) =>
      b
        .setName("search")
        .setDescription("Search for tracks and pick which to queue")
        .addStringOption((o) => o.setName("query").setDescription("What to search for").setRequired(true))
        .addStringOption((o) =>
          o
            .setName("source")
            .setDescription("Where to search; YouTube if not given")
            .addChoices(...SEARCH_SOURCES),
        ),
    async (interaction, engine) => {
      const player = joinedPlayer(interaction, engine);
      await interaction.deferReply();
      const prefix = interaction.options.getString("source") ?? undefined;
      const { tracks } = await engine.manager.search(interaction.options.getString("query", true), {
        ...(prefix === undefined ? {} : { prefix }),
        limit: 10,
      });
      if (tracks.length === 0) {
        throw new UserError("Nothing found.");
      }
      const source = SEARCH_SOURCES.find((s) => s.value === (prefix ?? "ytsearch"))?.name ?? prefix ?? "YouTube";
      const menu = new StringSelectMenuBuilder()
        .setCustomId("pick")
        .setPlaceholder("Pick tracks to queue")
        .setMinValues(1)
        .setMaxValues(tracks.length)
        .addOptions(
          tracks.map((track, index) => ({
            label: truncate(`${index + 1}. ${track.info.title}`, 100),
            description: truncate([track.info.author, trackLength(track)].filter(Boolean).join(" · "), 100),
            value: String(index),
          })),
        );
      const message = await interaction.editReply({
        ...(await searchMessage(engine.manager, tracks, source)),
        components: [new ActionRowBuilder<StringSelectMenuBuilder>().addComponents(menu)],
      });

      const picked = await message
        .awaitMessageComponent({
          componentType: ComponentType.StringSelect,
          time: PICK_TIMEOUT_MS,
          filter: (pick) => pick.user.id === interaction.user.id,
        })
        .catch(() => null);
      if (!picked) {
        await interaction.editReply({ content: "Nothing picked.", components: [] });
        return;
      }
      await picked.deferUpdate();
      const chosen = picked.values.flatMap((value) => tracks[Number(value)] ?? []);
      const idle = player.track === null;
      const added = await player.queue.add(chosen, {
        ...(idle ? { index: 0 } : {}),
        userData: requester(interaction),
      });
      await interaction.editReply({
        content: addedMessage(added.items, null, added.index, idle),
        embeds: [],
        components: [],
        attachments: [],
      });
    },
  ),
];

/** Each user's latest autocomplete request; earlier ones are answered without searching. */
const latestSuggestions = new Map<string, string>();

/** Search results for what is typed so far; their links are what gets played. */
async function suggestTracks(interaction: GuildAutocomplete, engine: Engine): Promise<void> {
  const text = interaction.options.getFocused().trim();
  const user = interaction.user.id;
  latestSuggestions.set(user, interaction.id);
  let choices: Array<{ name: string; value: string }> = [];
  try {
    await Bun.sleep(SUGGESTION_DELAY_MS);
    const latest = latestSuggestions.get(user) === interaction.id;
    if (latest && text.length >= 3 && !/^https?:\/\//i.test(text)) {
      const [, prefix, terms] = /^([a-z]+search):(.*)$/i.exec(text) ?? [];
      const search = engine.manager.search(terms ?? text, {
        ...(prefix === undefined ? {} : { prefix: prefix.toLowerCase() }),
        limit: 10,
      });
      const result = await withTimeout(search, SUGGESTION_TIMEOUT_MS - SUGGESTION_DELAY_MS).catch(() => null);
      choices = (result?.tracks ?? [])
        // a choice's value holds at most 100 characters
        .filter((track) => track.info.uri.length <= 100)
        .map((track) => ({ name: truncate(plainTrack(track), 100), value: track.info.uri }));
    }
  } finally {
    if (latestSuggestions.get(user) === interaction.id) {
      latestSuggestions.delete(user);
    }
  }
  await interaction.respond(choices);
}

/** `/play`'s and `/next`'s part of the track to play. */
function timeOption(o: SlashCommandStringOption): SlashCommandStringOption {
  return o
    .setName("time")
    .setDescription("Part to play: 1:30 (from), 1:30-2:45 (from-to) or -2:45 (to); else the link's own, like ?t=90")
    .setMaxLength(40);
}

/**
 * Queues the command's `query` (from its `start_index`, for a playlist; between its `time`s, else
 * the link's own) at `index`, and says what it found. Added on its own, the query loads at once,
 * as `load` would load it.
 */
async function queueQuery(
  interaction: GuildInteraction,
  player: Player,
  index: number | undefined,
  started: boolean,
): Promise<void> {
  const start = interaction.options.getInteger("start_index");
  const time = interaction.options.getString("time");
  const range = time === null ? null : parseTimeRange(time);
  if (time !== null && range === null) {
    throw new UserError(TIME_RANGE_HELP);
  }
  const added = await player.queue.add(asQuery(interaction.options.getString("query", true)), {
    ...(index === undefined ? {} : { index }),
    ...(start === null ? {} : { startIndex: start - 1 }),
    // a side not given stays as the link has it
    ...(range?.startTime === undefined ? {} : { startTime: range.startTime }),
    ...(range?.endTime === undefined ? {} : { endTime: range.endTime }),
    userData: requester(interaction),
  });
  const item = added.items[0];
  if (!item) {
    throw new UserError("Nothing to queue.");
  }
  const { items, playlist } = await player.queue.whenLoaded(item.id);
  await interaction.editReply(addedMessage(items, playlist, added.index, started));
}

/** The list a `/bulk` file holds. */
async function readList(file: Attachment): Promise<string> {
  if (file.size > MAX_LIST_BYTES) {
    throw new UserError(`The file is larger than ${MAX_LIST_BYTES / 1024} KiB.`);
  }
  if (file.contentType !== null && !file.contentType.startsWith("text/")) {
    throw new UserError("Attach a text file, with one link or search per line.");
  }
  const response = await fetch(file.url);
  if (!response.ok) {
    throw new UserError(`Discord answered HTTP ${response.status} for the file.`);
  }
  return response.text();
}

/** Queues a list's lines (from the command's `start_index`) and reports on them as they load. */
async function queueList(interaction: GuildInteraction, engine: Engine, text: string, edit: Edit): Promise<void> {
  const start = interaction.options.getInteger("start_index") ?? 1;
  const lines = parseQueries(text);
  const queries = lines.slice(start - 1).map(asQuery);
  if (queries.length === 0) {
    throw new UserError(lines.length === 0 ? "The list is empty." : `The list has only ${plural(lines.length, "line")}.`);
  }
  if (queries.length > MAX_LINES) {
    throw new UserError(`A list holds at most ${MAX_LINES} lines; this one has ${queries.length}.`);
  }
  const player = joinedPlayer(interaction, engine);
  const idle = player.track === null;
  const added = await player.queue.add(queries, {
    ...(idle ? { index: 0 } : {}),
    userData: requester(interaction),
  });
  // waiting starts before the reply goes out, so that loads finishing meanwhile are not missed
  const loads = added.items.map((item) =>
    player.queue.whenLoaded(item.id).then(
      (loaded): LineOutcome => ({ tracks: loaded.items.length }),
      (error: unknown): LineOutcome => ({ error }),
    ),
  );
  const from = start > 1 ? ` from line ${start}` : "";
  await edit(`⏳ Queued ${plural(queries.length, "line")}${from}${idle ? ", playing the first" : ""}; loading them…`);
  void summarize(engine, player, edit, loads, queries, start);
}

/**
 * Edits `/bulk`'s reply as its lines load, ending with what was queued and what failed. A long
 * list can outlast the reply (Discord's 15 minutes): the summary then goes to the player's
 * channel.
 */
async function summarize(
  engine: Engine,
  player: Player,
  edit: Edit,
  loads: Array<Promise<LineOutcome>>,
  queries: string[],
  firstLine: number,
): Promise<void> {
  let done = 0;
  let tracks = 0;
  let shown = Date.now();
  const failures: Array<{ line: number; text: string }> = [];
  await Promise.all(
    loads.map(async (load, i) => {
      const outcome = await load;
      const line = firstLine + i;
      if ("error" in outcome) {
        const query = queries[i] ?? "";
        const shownQuery = /^https?:\/\//.test(query) ? `<${query}>` : `\`${query.replaceAll("`", "'")}\``;
        failures.push({ line, text: `${line}. ${shownQuery}: ${loadFailure(outcome.error)}` });
      } else {
        tracks += outcome.tracks;
      }
      done += 1;
      if (done < loads.length && Date.now() - shown >= PROGRESS_INTERVAL_MS) {
        shown = Date.now();
        await edit(`⏳ Loading… ${done} of ${plural(loads.length, "line")} done, ${plural(tracks, "track")} queued`);
      }
    }),
  );
  failures.sort((a, b) => a.line - b.line);
  const summary = `✅ Queued ${plural(tracks, "track")} from ${plural(loads.length, "line")}`;
  const failed = failures.length === 0 ? "." : `; ${failures.length} failed:\n${failures.map((f) => f.text).join("\n")}`;
  const content = truncate(`${summary}${failed}`, MAX_MESSAGE);
  if (!(await edit(content))) {
    engine.messages.post(player, content, failures.length === 0 ? "routine" : "problem");
  }
}

/** Why a query did not load: the engine's reason, which is no fault of the bot's. */
function loadFailure(error: unknown): string {
  return error instanceof EntroximerError ? error.message : failureText(error, "loading a query");
}

/** Wraps a reply's `editReply`, keeping mentions from pinging anyone. */
function editor(editReply: (options: { content: string; allowedMentions: { parse: [] } }) => Promise<unknown>): Edit {
  return async (content) => {
    try {
      await editReply({ content, allowedMentions: { parse: [] } });
      return true;
    } catch (error) {
      console.warn("cannot update a reply:", error);
      return false;
    }
  };
}

/**
 * Web URLs, Spotify URIs and searches (`scsearch:...`) are loaded as they are; anything else
 * searches YouTube. Nothing else gets through, so users cannot make the engine open files on its
 * host.
 */
function asQuery(text: string): string {
  const query = text.trim();
  const passes = [/^https?:\/\//i, /^spotify:/i, /^[a-z]+search\d*:/i].some((pattern) => pattern.test(query));
  return passes ? query : `ytsearch:${query}`;
}

/** What a command queued: `tracks` (from `playlist`) at `index`. */
function addedMessage(
  tracks: readonly QueueItem[],
  playlist: PlaylistInfo | null,
  index: number,
  started: boolean,
): string {
  const first = tracks[0];
  if (!first) {
    return "Nothing to queue.";
  }
  const what =
    tracks.length === 1 && playlist === null
      ? describeTrack(first.track)
      : `${plural(tracks.length, "track")}${playlist ? ` from **${escape(playlist.name)}**` : ""}, starting with ${describeTrack(first.track)}`;
  if (started) {
    return `Playing ${what}`;
  }
  return index === 0 ? `Up next: ${what}` : `Queued ${what} at position ${index + 1}`;
}

function plural(count: number, noun: string): string {
  return `${count} ${noun}${count === 1 ? "" : "s"}`;
}
