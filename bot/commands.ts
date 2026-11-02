import {
  ActionRowBuilder,
  ChannelType,
  ComponentType,
  DiscordAPIError,
  InteractionContextType,
  MessageFlags,
  RESTJSONErrorCodes,
  SlashCommandBuilder,
  StringSelectMenuBuilder,
} from "discord.js";
import type { AutocompleteInteraction, ChatInputCommandInteraction, SharedSlashCommand } from "discord.js";
import { EntroximerError, append, parseChain } from "entroximer-client";
import type { PlaylistInfo, Player, QueueAdded, QueueItem, Repeat, Track } from "entroximer-client";

import { nowPlayingMessage, queueMessage, searchMessage } from "./embeds";
import type { Engine } from "./engine";
import { describeTrack, escape, formatTime, parseTime, plainTrack, trackLength, truncate } from "./format";

type GuildInteraction = ChatInputCommandInteraction<"cached">;
type GuildAutocomplete = AutocompleteInteraction<"cached">;

interface Command {
  data: SharedSlashCommand;
  execute(interaction: GuildInteraction, engine: Engine): Promise<void>;
  /** Suggests values for the option being typed. */
  autocomplete?(interaction: GuildAutocomplete, engine: Engine): Promise<void>;
}

/** A failure to explain to the user. */
class UserError extends Error {}

function command(
  build: (builder: SlashCommandBuilder) => SharedSlashCommand,
  execute: Command["execute"],
  autocomplete?: Command["autocomplete"],
): Command {
  const data = build(new SlashCommandBuilder().setContexts(InteractionContextType.Guild));
  return autocomplete ? { data, execute, autocomplete } : { data, execute };
}

/** Discord waits 3 s for suggestions. */
const SUGGESTION_TIMEOUT_MS = 2_500;
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

export const commands: Command[] = [
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
      engine.manager.player(interaction.guildId).connect(channelId);
      await interaction.reply(`Joining <#${channelId}>.`);
    },
  ),
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
      await interaction.reply(next ? `${what}; up now: ${describeTrack(next.track)}` : `${what}. The queue is empty.`);
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
      await interaction.reply(`Removed ${describeTrack(item.track)}.`);
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
      await interaction.reply(`Moved ${describeTrack(item.track)} to position ${to}.`);
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
    (b) =>
      b
        .setName("seek")
        .setDescription("Jump to a position in the current track")
        .addStringOption((o) =>
          o.setName("position").setDescription("Like 90, 1:30 or 1:02:03").setRequired(true),
        ),
    async (interaction, engine) => {
      const position = parseTime(interaction.options.getString("position", true));
      if (position === null) {
        throw new UserError("Give the position as seconds, `m:ss` or `h:mm:ss`.");
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
    (b) =>
      b
        .setName("volume")
        .setDescription("Set the volume")
        .addIntegerOption((o) =>
          o
            .setName("percent")
            .setDescription("100 leaves the audio unchanged")
            .setMinValue(0)
            .setMaxValue(1000)
            .setRequired(true),
        ),
    async (interaction, engine) => {
      const percent = interaction.options.getInteger("percent", true);
      await existingPlayer(interaction, engine).setVolume(percent);
      await interaction.reply(`Volume set to ${percent}%.`);
    },
  ),
  command(
    (b) =>
      b
        .setName("effects")
        .setDescription("Apply a preset, an effect chain or both; with neither, remove effects")
        .addStringOption((o) =>
          o.setName("preset").setDescription("A ready-made effect, like nightcore or bass_boost").setAutocomplete(true),
        )
        .addStringOption((o) =>
          o.setName("chain").setDescription("Like lowpass(800) => reverb(10, 2); with a preset, added after it"),
        ),
    async (interaction, engine) => {
      const preset = interaction.options.getString("preset")?.trim() || null;
      const text = interaction.options.getString("chain")?.trim() ?? "";
      const player = existingPlayer(interaction, engine);
      if (preset === null && text === "") {
        await player.setGraph(null);
        await interaction.reply("Effects removed.");
        return;
      }
      const { nodes } = await engine.manager.info();
      // the chain is checked before anything changes, so a typo leaves the effects alone
      const extra = parseChain(text, nodes).nodes.map(({ type, params }) => ({ type, params }));
      const base = preset === null ? null : await engine.manager.buildPreset(preset);
      await player.setGraph(append(base, ...extra));
      const parts = [...(preset === null ? [] : [`preset **${escape(preset)}**`]), ...(text ? [`\`${text}\``] : [])];
      await interaction.reply(`Effects: ${parts.join(" then ")}`);
    },
    suggestPresets,
  ),
  command(
    (b) => b.setName("stop").setDescription("Stop playback; the queue waits until something plays"),
    async (interaction, engine) => {
      await existingPlayer(interaction, engine).stop();
      await interaction.reply("Stopped. `/skip` continues with the queue.");
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
  command(
    (b) => b.setName("leave").setDescription("Stop playback and leave the voice channel"),
    async (interaction, engine) => {
      await existingPlayer(interaction, engine).destroy();
      await interaction.reply("Left the voice channel.");
    },
  ),
  command(
    (b) => b.setName("engine").setDescription("Show the engine's version and load"),
    async (interaction, engine) => {
      const [info, stats] = await Promise.all([engine.manager.info(), engine.manager.stats()]);
      const mib = (bytes: number) => `${(bytes / 2 ** 20).toFixed(1)} MiB`;
      const searches = info.providers.flatMap((provider) => provider.searchPrefixes);
      await interaction.reply(
        [
          `Entroximer ${info.version}, protocol ${info.protocol}, yt-dlp ${info.ytdlp ?? "unknown"}`,
          `YouTube session: ${info.youtubeSession}`,
          `${stats.playingPlayers} of ${stats.players} players playing, up ${formatTime(stats.uptime)}`,
          `memory ${mib(stats.memoryBytes)}, cache ${mib(stats.cacheMemoryBytes)} in RAM and ${mib(stats.cacheDiskBytes)} on disk`,
          `sources: ${info.providers.map((provider) => provider.displayName).join(", ")}`,
          `searches: ${searches.join(", ")}`,
          `effects: ${info.nodes.map((node) => node.type).join(", ")}`,
          `presets: ${info.presets.map((preset) => preset.name).join(", ")}`,
        ].join("\n"),
      );
    },
  ),
];

/** Runs a command, answering failures with an explanation only its user sees. */
export async function run(interaction: GuildInteraction, engine: Engine): Promise<void> {
  const found = commands.find((c) => c.data.name === interaction.commandName);
  if (!found) {
    return;
  }
  engine.announceIn(interaction.guildId, interaction.channel?.isSendable() ? interaction.channel : null);
  try {
    await found.execute(interaction, engine);
  } catch (error) {
    if (answeredElsewhere(error)) {
      warnAnsweredElsewhere();
      return;
    }
    const message =
      error instanceof UserError || error instanceof EntroximerError
        ? error.message
        : "Something went wrong; see the bot's log.";
    if (!(error instanceof UserError)) {
      console.error(`/${interaction.commandName} failed:`, error);
    }
    const reply = { content: `❌ ${message}`, embeds: [], components: [], allowedMentions: { parse: [] } };
    if (interaction.deferred || interaction.replied) {
      await interaction.editReply({ ...reply, attachments: [] });
    } else {
      await interaction.reply({ ...reply, flags: MessageFlags.Ephemeral });
    }
  }
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

/** Answers an autocomplete request; without suggestions if they take too long. */
export async function suggest(interaction: GuildAutocomplete, engine: Engine): Promise<void> {
  const found = commands.find((c) => c.data.name === interaction.commandName);
  await found?.autocomplete?.(interaction, engine);
}

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

/** The engine's presets whose names contain what is typed so far. */
async function suggestPresets(interaction: GuildAutocomplete, engine: Engine): Promise<void> {
  const typed = interaction.options.getFocused().trim().toLowerCase();
  const { presets } = await withTimeout(engine.manager.info(), SUGGESTION_TIMEOUT_MS).catch(() => ({ presets: [] }));
  const choices = presets
    .filter((preset) => preset.name.includes(typed))
    .slice(0, 25)
    .map((preset) => ({ name: truncate(`${preset.name}: ${preset.description}`, 100), value: preset.name }));
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

function requester(interaction: GuildInteraction): { requester: string } {
  return { requester: interaction.user.id };
}

function isRepeat(text: string): text is Repeat {
  return text === "off" || text === "track" || text === "queue";
}

function itemAt(player: Player, position: number): QueueItem {
  const item = player.queue.items[position - 1];
  if (!item) {
    throw new UserError(`The queue has ${player.queue.length} tracks.`);
  }
  return item;
}

function voiceChannelOf(interaction: GuildInteraction): string {
  const channelId = interaction.member.voice.channelId;
  if (!channelId) {
    throw new UserError("Join a voice channel first, or name one.");
  }
  return channelId;
}

/** The guild's player, joining the user's voice channel if it is not in one yet. */
function joinedPlayer(interaction: GuildInteraction, engine: Engine): Player {
  const player = engine.manager.player(interaction.guildId);
  if (player.channelId === null) {
    player.connect(voiceChannelOf(interaction));
  }
  return player;
}

function existingPlayer(interaction: GuildInteraction, engine: Engine): Player {
  const player = engine.manager.players.get(interaction.guildId);
  if (!player) {
    throw new UserError("Nothing is playing here.");
  }
  return player;
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

function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`no answer within ${ms} ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}
