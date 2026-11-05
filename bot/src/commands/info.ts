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
      // e.g. "840.0 of 3072 MiB"; limits are whole MiB
      const ofLimit = (bytes: number, limit: number) => `${(bytes / 2 ** 20).toFixed(1)} of ${Math.round(limit / 2 ** 20)} MiB`;
      const disk =
        stats.cacheDiskLimitBytes === 0
          ? "nothing on disk (RAM only)"
          : `${ofLimit(stats.cacheDiskBytes, stats.cacheDiskLimitBytes)} on disk`;
      const loop =
        stats.loopCacheLimitBytes === 0
          ? "no loop cache"
          : `${ofLimit(stats.loopCacheBytes, stats.loopCacheLimitBytes)} kept for repeated queues`;
      const searches = info.providers.flatMap((provider) => provider.searchPrefixes);
      await interaction.reply(
        [
          `Entroximer ${info.version}, protocol ${info.protocol}, yt-dlp ${info.ytdlp ?? "unknown"}`,
          `YouTube session: ${info.youtubeSession}`,
          `${stats.playingPlayers} of ${stats.players} players playing, up ${formatTime(stats.uptime)}`,
          `memory ${mib(stats.memoryBytes)}, cache ${ofLimit(stats.cacheMemoryBytes, stats.cacheMemoryLimitBytes)} in RAM and ${disk}, ${loop}`,
          `sources: ${info.providers.map((provider) => provider.displayName).join(", ")}`,
          `searches: ${searches.join(", ")}`,
          `effects: ${info.nodes.map((node) => node.type).join(", ")}`,
          `presets: ${info.presets.map((preset) => preset.name).join(", ")}`,
        ].join("\n"),
      );
    },
  ),
];
