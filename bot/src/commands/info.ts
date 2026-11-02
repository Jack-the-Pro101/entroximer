// The engine itself: /engine.
import { formatTime } from "../format";
import { command } from "./command";
import type { Command } from "./command";

export const infoCommands: Command[] = [
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
