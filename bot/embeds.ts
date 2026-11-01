// Embeds for what is playing, the queue and search results, with the tracks' artwork.
import { EmbedBuilder } from "discord.js";
import type { Player, Track } from "entroximer-client";

import { describeTrack, escape, formatTime, truncate } from "./format";

const COLOR = 0x2bb3a0;
/** Queue items per page of `/queue`. */
export const QUEUE_PAGE = 10;

export function nowPlayingEmbed(player: Player, track: Track): EmbedBuilder {
  const { title, uri, author, length, artworkUrl, isStream } = track.info;
  const position = player.position;
  const embed = new EmbedBuilder()
    .setColor(COLOR)
    .setAuthor({ name: player.paused ? "Paused" : "Now playing" })
    .setTitle(truncate(title, 256))
    .setThumbnail(artworkUrl);
  if (/^https?:\/\//.test(uri)) {
    embed.setURL(uri);
  }
  if (author) {
    embed.setDescription(`by ${escape(author)}`);
  }
  const progress =
    isStream || length === null
      ? formatTime(position)
      : `${progressBar(position / length)} ${formatTime(position)} / ${formatTime(length)}`;
  embed.addFields({ name: isStream ? "Live for" : "Position", value: progress });

  const next = player.queue.items[0];
  embed.addFields({
    name: "Up next",
    value: next ? truncate(describeTrack(next.track), 1024) : "Nothing queued",
  });
  const buffered = player.state?.buffered ?? [];
  const details = [
    `volume ${player.volume}%`,
    `repeat ${player.queue.repeat}`,
    ...(player.state?.buffering ? ["buffering"] : []),
    ...(buffered.length > 0
      ? [`downloaded ${buffered.map(([start, end]) => `${formatTime(start)}–${formatTime(end)}`).join(", ")}`]
      : []),
  ];
  return embed.setFooter({ text: details.join(" · ") });
}

/** Page `page` (from 1) of the queue; pages past the end show the last one. */
export function queueEmbed(player: Player, page: number): EmbedBuilder {
  const items = player.queue.items;
  const pages = Math.max(1, Math.ceil(items.length / QUEUE_PAGE));
  const shown = Math.min(Math.max(1, page), pages);
  const first = (shown - 1) * QUEUE_PAGE;
  const lines = items
    .slice(first, first + QUEUE_PAGE)
    .map((item, index) => truncate(`${first + index + 1}. ${describeTrack(item.track)}`, 400));

  const embed = new EmbedBuilder()
    .setColor(COLOR)
    .setTitle("Queue")
    .setDescription(lines.length > 0 ? lines.join("\n") : "Nothing queued.");
  if (player.track) {
    embed
      .addFields({ name: "Now playing", value: truncate(describeTrack(player.track), 1024) })
      .setThumbnail(player.track.info.artworkUrl);
  }
  const known = items.filter((item) => item.track.info.length !== null);
  const total = known.reduce((sum, item) => sum + (item.track.info.length ?? 0), 0);
  const duration = `${formatTime(total)}${known.length < items.length ? "+" : ""}`;
  return embed.setFooter({
    text: `Page ${shown} of ${pages} · ${items.length} tracks · ${duration} · repeat ${player.queue.repeat}`,
  });
}

export function searchEmbed(tracks: readonly Track[], source: string): EmbedBuilder {
  return new EmbedBuilder()
    .setColor(COLOR)
    .setTitle(`Results from ${source}`)
    .setDescription(
      tracks.map((track, index) => truncate(`${index + 1}. ${describeTrack(track)}`, 400)).join("\n"),
    )
    .setThumbnail(tracks[0]?.info.artworkUrl ?? null);
}

/** `▬▬▬🔘▬▬▬▬▬▬` for a fraction from 0 to 1. */
function progressBar(fraction: number): string {
  const width = 12;
  const at = Math.min(width - 1, Math.max(0, Math.floor(fraction * width)));
  return `${"▬".repeat(at)}🔘${"▬".repeat(width - 1 - at)}`;
}
