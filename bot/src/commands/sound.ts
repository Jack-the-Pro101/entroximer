// How it sounds, and sounds over the music: /volume, /effects, /sfx, /announce.
import { append, parseChain } from "entroximer-client";
import type { Layer, Player } from "entroximer-client";

import type { Engine } from "../engine";
import { describeTrack, escape, truncate } from "../format";
import { SUGGESTION_TIMEOUT_MS, UserError, command, withTimeout } from "./command";
import type { Command, GuildAutocomplete, GuildInteraction } from "./command";
import { existingPlayer, joinedPlayer, requester } from "./players";

/** Layers for sounds besides the music, which play over it with their own effects and volume. */
const SFX = "sfx";
const ANNOUNCEMENTS = "announcements";
/** The options of commands that play a sound; see `soundSource`. */
const FILE_OPTION = "An audio file";
const URL_OPTION = "A link to an audio file or page";
const LAYER_CHOICES = [
  { name: "Music", value: "main" },
  { name: "Sound effects", value: SFX },
  { name: "Announcements", value: ANNOUNCEMENTS },
];

export const soundCommands: Command[] = [
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
        )
        .addStringOption((o) =>
          o
            .setName("layer")
            .setDescription("What to change; the music if absent")
            .addChoices(...LAYER_CHOICES),
        ),
    async (interaction, engine) => {
      const percent = interaction.options.getInteger("percent", true);
      await layerOf(interaction, existingPlayer(interaction, engine)).setVolume(percent);
      await interaction.reply(`Volume of ${layerName(interaction)} set to ${percent}%.`);
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
        )
        .addStringOption((o) =>
          o
            .setName("layer")
            .setDescription("What to change; the music if absent")
            .addChoices(...LAYER_CHOICES),
        ),
    async (interaction, engine) => {
      const preset = interaction.options.getString("preset")?.trim() || null;
      const text = interaction.options.getString("chain")?.trim() ?? "";
      const target = layerOf(interaction, existingPlayer(interaction, engine));
      const of = layerName(interaction);
      if (preset === null && text === "") {
        await target.setGraph(null);
        await interaction.reply(`Effects of ${of} removed.`);
        return;
      }
      const { nodes } = await engine.manager.info();
      // the chain is checked before anything changes, so a typo leaves the effects alone
      const extra = parseChain(text, nodes).nodes.map(({ type, params }) => ({ type, params }));
      const base = preset === null ? null : await engine.manager.buildPreset(preset);
      await target.setGraph(append(base, ...extra));
      const parts = [...(preset === null ? [] : [`preset **${escape(preset)}**`]), ...(text ? [`\`${text}\``] : [])];
      await interaction.reply(`Effects of ${of}: ${parts.join(" then ")}`);
    },
    suggestPresets,
  ),
  command(
    (b) =>
      b
        .setName("sfx")
        .setDescription("Sound effects, played over the music")
        .addSubcommand((s) =>
          s
            .setName("play")
            .setDescription("Play a sound over the music")
            .addAttachmentOption((o) => o.setName("file").setDescription(FILE_OPTION))
            .addStringOption((o) => o.setName("url").setDescription(URL_OPTION))
            .addBooleanOption((o) => o.setName("loop").setDescription("Play it again and again, until /sfx stop"))
            .addIntegerOption((o) =>
              o.setName("volume").setDescription("Percent; 100 if absent").setMinValue(0).setMaxValue(1000),
            ),
        )
        .addSubcommand((s) => s.setName("stop").setDescription("Stop the sound effects")),
    async (interaction, engine) => {
      if (interaction.options.getSubcommand() === "stop") {
        await existingPlayer(interaction, engine).layer(SFX).stop();
        await interaction.reply("Stopped the sound effects.");
        return;
      }
      const source = soundSource(interaction);
      const loop = interaction.options.getBoolean("loop") ?? false;
      const volume = interaction.options.getInteger("volume");
      const player = joinedPlayer(interaction, engine);
      const sfx = player.layer(SFX);
      // sounds play over each other here, as many as 8
      if (sfx.mode !== "mix") {
        await sfx.configure({ mode: "mix" });
      }
      const clip = await sfx.play(source, {
        loop,
        userData: requester(interaction),
        ...(volume === null ? {} : { volume }),
      });
      const looping = loop ? ", again and again until `/sfx stop`" : "";
      await interaction.reply(`🔊 Playing ${describeTrack(clip.track)} over the music${looping}.`);
    },
  ),
  command(
    (b) =>
      b
        .setName("announce")
        .setDescription("Play an announcement; the music goes quiet while it plays")
        .addAttachmentOption((o) => o.setName("file").setDescription(FILE_OPTION))
        .addStringOption((o) => o.setName("url").setDescription(URL_OPTION)),
    async (interaction, engine) => {
      const source = soundSource(interaction);
      const player = joinedPlayer(interaction, engine);
      const announcements = player.layer(ANNOUNCEMENTS);
      // one after another, each lowering the music to a quarter
      if (announcements.duck === null) {
        await announcements.configure({ duck: { layers: ["main"], volume: 25 } });
      }
      const clip = await announcements.play(source, { userData: requester(interaction) });
      await interaction.reply(
        clip.status === "playing"
          ? `📢 Announcing ${describeTrack(clip.track)}.`
          : `📢 ${describeTrack(clip.track)} plays after the announcements before it.`,
      );
    },
  ),
];

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

/** The sound a command plays: an attached file (from Discord's CDN), or a link. */
function soundSource(interaction: GuildInteraction): string {
  const file = interaction.options.getAttachment("file");
  if (file) {
    if (file.contentType && !/^(audio|video)\//.test(file.contentType)) {
      throw new UserError("That file is not audio.");
    }
    return file.url;
  }
  const url = interaction.options.getString("url")?.trim();
  if (url && /^https?:\/\//i.test(url)) {
    return url;
  }
  throw new UserError("Attach an audio file, or give a link to one.");
}

/** What `/volume` and `/effects` change: the music, or one of the other layers. */
function layerOf(interaction: GuildInteraction, player: Player): Player | Layer {
  const name = interaction.options.getString("layer") ?? "main";
  return name === "main" ? player : player.layer(name);
}

function layerName(interaction: GuildInteraction): string {
  const name = interaction.options.getString("layer") ?? "main";
  const choice = LAYER_CHOICES.find((layer) => layer.value === name)?.name ?? name;
  return choice.toLowerCase().replace(/^music$/, "the music");
}
