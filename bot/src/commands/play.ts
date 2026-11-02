// Finding tracks and queueing them: /play, /next, /search.
import { ActionRowBuilder, ComponentType, StringSelectMenuBuilder } from "discord.js";
import type { PlaylistInfo, QueueAdded, Track } from "entroximer-client";

import { searchMessage } from "../embeds";
import type { Engine } from "../engine";
import { describeTrack, escape, plainTrack, trackLength, truncate } from "../format";
import { SUGGESTION_TIMEOUT_MS, UserError, command, withTimeout } from "./command";
import type { Command, GuildAutocomplete } from "./command";
import { joinedPlayer, requester } from "./players";

/** Suggestions wait this long for more typing, so that only what is typed last is searched. */
const SUGGESTION_DELAY_MS = 350;
/** How long `/search` waits for a pick. */
const PICK_TIMEOUT_MS = 60_000;

const SEARCH_SOURCES = [
  { name: "YouTube", value: "ytsearch" },
  { name: "YouTube Music", value: "ytmsearch" },
  { name: "SoundCloud", value: "scsearch" },
  // only if the engine has a Spotify app configured; plays from YouTube
  { name: "Spotify", value: "spsearch" },
];

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
        ),
    async (interaction, engine) => {
      const player = joinedPlayer(interaction, engine);
      await interaction.deferReply();
      const found = await tracksFor(engine, interaction.options.getString("query", true));
      const idle = player.track === null;
      // with nothing playing, the front of the queue starts
      const added = await player.queue.add(found.tracks, {
        ...(idle ? { index: 0 } : {}),
        userData: requester(interaction),
      });
      await interaction.editReply(addedMessage(added, found.playlist, idle));
    },
    suggestTracks,
  ),
  command(
    (b) =>
      b
        .setName("next")
        .setDescription("Queue a track to play right after the current one")
        .addStringOption((o) =>
          o
            .setName("query")
            .setDescription("A URL, or words to search YouTube for")
            .setRequired(true)
            .setAutocomplete(true),
        ),
    async (interaction, engine) => {
      const player = joinedPlayer(interaction, engine);
      await interaction.deferReply();
      const found = await tracksFor(engine, interaction.options.getString("query", true));
      const idle = player.track === null;
      const added = await player.queue.add(found.tracks, { index: 0, userData: requester(interaction) });
      await interaction.editReply(addedMessage(added, found.playlist, idle));
    },
    suggestTracks,
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
        content: addedMessage(added, undefined, idle),
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

/** The tracks a query stands for: a track, a playlist from the track its link points at, or a search's first result. */
async function tracksFor(engine: Engine, text: string): Promise<{ tracks: Track[]; playlist?: PlaylistInfo }> {
  const loaded = await engine.manager.load(asQuery(text));
  switch (loaded.type) {
    case "track":
      return { tracks: [loaded.track] };
    case "playlist":
      if (loaded.tracks.length === 0) {
        throw new UserError(`The playlist **${escape(loaded.playlist.name)}** is empty.`);
      }
      return { tracks: loaded.tracks.slice(loaded.playlist.selectedTrack ?? 0), playlist: loaded.playlist };
    case "search": {
      const first = loaded.tracks[0];
      if (!first) {
        throw new UserError("Nothing found.");
      }
      return { tracks: [first] };
    }
    case "empty":
      throw new UserError("Nothing found.");
  }
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

function addedMessage(added: QueueAdded, playlist: PlaylistInfo | undefined, started: boolean): string {
  const first = added.items[0];
  if (!first) {
    return "Nothing to queue.";
  }
  const what =
    added.items.length === 1
      ? describeTrack(first.track)
      : `${added.items.length} tracks${playlist ? ` from **${escape(playlist.name)}**` : ""}, starting with ${describeTrack(first.track)}`;
  if (started) {
    return `Playing ${what}`;
  }
  return added.index === 0 ? `Up next: ${what}` : `Queued ${what} at position ${added.index + 1}`;
}
